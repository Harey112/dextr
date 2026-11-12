use std::{collections::BTreeMap, path::PathBuf};

use anyhow::Context as _;
use dextr_proto::{DEFAULT_PORT, Os, Rect, Side};
use serde::{Deserialize, Serialize};

use crate::{
    settings::{
        Appearance, Channel, ClipboardSettings, ConnectionSettings, DeviceRules, Direction, DisplayLayout, KeyboardSettings,
        MouseSettings, PeerClipboard, PeerKeyboard, PeerMouse, SwitchingSettings,
    },
    world::{Arrangement, DeviceId},
};

/// A paired device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct PeerEntry {
    /// Its name, as last seen.
    pub name: String,
    /// Where to reach it: `host` or `host:port`.
    pub address: String,
    pub os: Option<Os>,
    /// Its screens from the last connection, so the arrangement can show it
    /// while it's offline.
    pub screens: Vec<Rect>,
    /// What goes from this device to it.
    pub send: Direction,
    /// What comes from it to this device.
    pub receive: Direction,
    /// This mouse on it.
    pub mouse: PeerMouse,
    /// This keyboard on it.
    pub keyboard: PeerKeyboard,
    /// The clipboard with it.
    pub clipboard: PeerClipboard,
}

impl PeerEntry {
    /// Whether `channel` is shared with this device in one direction: its
    /// switch and that direction's switch must both be on.
    pub fn allows(&self, channel: Channel, outgoing: bool) -> bool {
        let on = match channel {
            Channel::Mouse => self.mouse.on,
            Channel::Keyboard => self.keyboard.on,
            Channel::Clipboard => self.clipboard.on,
        };
        on && if outgoing { self.send.get(channel) } else { self.receive.get(channel) }
    }
}

/// Settings file format: 2 moved mouse, keyboard and clipboard options into
/// each device.
const VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Format of the file, for moving old settings ([`VERSION`]).
    #[serde(default)]
    pub version: u32,
    /// Shown to other devices.
    pub device_name: String,
    /// UDP port this device listens on.
    pub port: u16,
    /// Paired devices, by certificate fingerprint. Only they may connect,
    /// and we only connect to them.
    pub peers: BTreeMap<DeviceId, PeerEntry>,
    /// Exact or proportional edges.
    pub layout: DisplayLayout,
    /// Where every device's screens sit (shared by all devices).
    pub arrangement: Arrangement,
    /// Where a newly paired device goes until it's arranged.
    pub new_device_side: Side,
    /// Saved RemoteDesktop portal grant, so the permission dialog is shown once.
    pub inject_restore_token: Option<String>,
    pub connection: ConnectionSettings,
    pub switching: SwitchingSettings,
    pub mouse: MouseSettings,
    pub keyboard: KeyboardSettings,
    pub clipboard: ClipboardSettings,
    pub appearance: Appearance,

    // Single-peer settings from earlier versions, read once and moved into
    // `peers` and `arrangement`.
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub peer_address: String,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub peer_fingerprint: String,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub peer_side: Option<Side>,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub last_peer_name: Option<String>,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub last_peer_screens: Vec<Rect>,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub devices: BTreeMap<String, DeviceRules>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION,
            device_name: hostname(),
            port: DEFAULT_PORT,
            peers: BTreeMap::new(),
            layout: DisplayLayout::Exact,
            arrangement: Arrangement::default(),
            new_device_side: Side::Right,
            inject_restore_token: None,
            connection: ConnectionSettings::default(),
            switching: SwitchingSettings::default(),
            mouse: MouseSettings::default(),
            keyboard: KeyboardSettings::default(),
            clipboard: ClipboardSettings::default(),
            appearance: Appearance::default(),
            peer_address: String::new(),
            peer_fingerprint: String::new(),
            peer_side: None,
            last_peer_name: None,
            last_peer_screens: Vec::new(),
            devices: BTreeMap::new(),
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
    /// The rules for a paired device (all allowed until changed).
    pub fn peer(&self, id: &str) -> PeerEntry {
        self.peers.get(id).cloned().unwrap_or_default()
    }

    /// Moves settings from the single-peer versions into `peers`.
    fn migrate(&mut self) {
        self.migrate_peers();
        if self.version < 2 {
            // Mouse, keyboard and clipboard options were the same for every
            // device: start each device with them.
            let m = &self.mouse;
            let mouse = PeerMouse { on: true, speed: m.speed, scroll_speed: m.scroll_speed, natural_scroll: m.natural_scroll, swap_buttons: m.swap_buttons };
            let keyboard = PeerKeyboard { on: true, modifiers: self.keyboard.modifiers };
            let clipboard = PeerClipboard { on: true, images: self.clipboard.images, max_mb: self.clipboard.max_mb };
            for p in self.peers.values_mut() {
                p.mouse = mouse.clone();
                p.keyboard = keyboard.clone();
                p.clipboard = clipboard.clone();
            }
        }
        self.version = VERSION;
    }

    fn migrate_peers(&mut self) {
        if let Some(side) = self.peer_side.take() {
            self.new_device_side = side;
        }
        let Some(fp) = crate::identity::parse(&std::mem::take(&mut self.peer_fingerprint)) else { return };
        let id = crate::identity::format(&fp);
        let rules = self.devices.remove(&id).unwrap_or_default();
        self.peers.entry(id).or_insert_with(|| PeerEntry {
            name: self.last_peer_name.take().unwrap_or_default(),
            address: std::mem::take(&mut self.peer_address),
            os: None,
            screens: std::mem::take(&mut self.last_peer_screens),
            send: rules.send,
            receive: rules.receive,
            ..PeerEntry::default()
        });
        self.devices.clear();
    }

    fn path() -> anyhow::Result<PathBuf> {
        Ok(dir()?.join("config.toml"))
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut config: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                config.migrate();
                Ok(config)
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_peer_config_moves_into_the_device_list() {
        let fp = "AB".repeat(32);
        let old = format!(
            "device_name = \"laptop\"\npeer_address = \"10.0.0.2\"\npeer_fingerprint = \"{fp}\"\npeer_side = \"Left\"\n\
             last_peer_name = \"Mac\"\nlayout = \"Proportional\"\n\n[mouse]\nspeed = 1.5\n\n[clipboard]\nimages = false\n\n[devices.\"{id}\".send]\nclipboard = false\n",
            id = crate::identity::format(&crate::identity::parse(&fp).unwrap()),
        );
        let mut c: Config = toml::from_str(&old).unwrap();
        c.migrate();
        let (id, peer) = c.peers.iter().next().unwrap();
        assert_eq!(id, &crate::identity::format(&crate::identity::parse(&fp).unwrap()));
        assert_eq!((peer.name.as_str(), peer.address.as_str()), ("Mac", "10.0.0.2"));
        assert!(!peer.send.clipboard && peer.send.mouse);
        assert_eq!(c.new_device_side, Side::Left);
        assert_eq!(c.layout, DisplayLayout::Proportional);
        assert_eq!(peer.mouse.speed, 1.5);
        assert!(!peer.clipboard.images);
        assert_eq!(c.version, VERSION);
        // Saved without the old fields; loads back the same.
        let again: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(again.peers, c.peers);
        assert!(!toml::to_string_pretty(&c).unwrap().contains("peer_fingerprint"));
    }
}
