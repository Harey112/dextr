//! User-adjustable behavior, stored in the config file.

use std::fmt;

use dextr_proto::{Key, Os};
use serde::{Deserialize, Serialize};

/// A fixed set of choices with labels and a default.
macro_rules! choice {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $label:literal),+ $(,)? } default $default:ident) => {
        $(#[$m])*
        #[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant,)+
        }
        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];
        }
        impl Default for $name {
            fn default() -> Self {
                $name::$default
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self { $($name::$variant => $label),+ })
            }
        }
    };
}

// ── Connection ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ConnectionSettings {
    /// Connect to the paired device when dextr starts.
    pub auto_connect: bool,
    /// Keep trying to reconnect after the connection drops.
    pub auto_reconnect: bool,
    /// Accept connections started by the paired device. When off, only this
    /// device can start a connection.
    pub allow_incoming: bool,
    /// Let the paired device's mouse and keyboard control this device.
    pub allow_control: bool,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self { auto_connect: true, auto_reconnect: true, allow_incoming: true, allow_control: true }
    }
}

// ── Switching ────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SwitchingSettings {
    /// Pixels at each end of the shared edge where crossing is ignored, so
    /// screen corners stay usable (hot corners, close buttons).
    pub dead_corners: u32,
    /// How long the pointer must push against the edge before switching, in
    /// milliseconds. Prevents accidental switches.
    pub delay_ms: u32,
}

impl Default for SwitchingSettings {
    fn default() -> Self {
        Self { dead_corners: 16, delay_ms: 0 }
    }
}

// ── Mouse ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MouseSettings {
    /// Master switch: share this mouse with other devices at all.
    pub enabled: bool,
    /// Before per-device settings: moved into each device's [`PeerMouse`].
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub speed: f32,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub scroll_speed: f32,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub natural_scroll: bool,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub swap_buttons: bool,
}

impl Default for MouseSettings {
    fn default() -> Self {
        Self { enabled: true, speed: 1.0, scroll_speed: 1.0, natural_scroll: false, swap_buttons: false }
    }
}

/// How this mouse behaves on one other device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct PeerMouse {
    /// Share the mouse with this device at all (both directions).
    pub on: bool,
    /// Multiplies pointer movement on that device.
    pub speed: f32,
    /// Multiplies scrolling on that device.
    pub scroll_speed: f32,
    /// Reverse scroll direction on that device.
    pub natural_scroll: bool,
    /// Swap the left and right buttons on that device.
    pub swap_buttons: bool,
}

impl Default for PeerMouse {
    fn default() -> Self {
        Self { on: true, speed: 1.0, scroll_speed: 1.0, natural_scroll: false, swap_buttons: false }
    }
}

impl PeerMouse {
    /// Scroll multiplier, negative for natural scrolling.
    pub fn scroll_factor(&self) -> f32 {
        if self.natural_scroll { -self.scroll_speed } else { self.scroll_speed }
    }
}

// ── Keyboard ─────────────────────────────────────────────────────────────

choice!(
    /// How modifier keys are sent to the other device.
    ModifierMap {
        Automatic => "Automatic",
        AsIs => "Send as they are",
        SwapCtrlMeta => "Swap Ctrl and Cmd/Super",
        SwapAltMeta => "Swap Alt and Cmd/Super",
    } default Automatic
);

choice!(
    /// Key combination that always brings the pointer back.
    ReturnShortcut {
        CtrlAltEsc => "Ctrl + Alt + Esc",
        ScrollLock => "Scroll Lock",
        Pause => "Pause",
    } default CtrlAltEsc
);

impl ReturnShortcut {
    /// Single keys that trigger the shortcut on their own.
    pub fn single_key(self) -> Option<Key> {
        match self {
            ReturnShortcut::CtrlAltEsc => None,
            ReturnShortcut::ScrollLock => Some(Key::keyboard(0x47)),
            ReturnShortcut::Pause => Some(Key::keyboard(0x48)),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct KeyboardSettings {
    /// Master switch: send this keyboard to other devices at all.
    pub enabled: bool,
    /// Works from every device.
    pub return_shortcut: ReturnShortcut,
    /// Before per-device settings: moved into each device's [`PeerKeyboard`].
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub modifiers: ModifierMap,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        Self { enabled: true, modifiers: ModifierMap::default(), return_shortcut: ReturnShortcut::default() }
    }
}

/// How this keyboard behaves on one other device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct PeerKeyboard {
    /// Share the keyboard with this device at all (both directions).
    pub on: bool,
    pub modifiers: ModifierMap,
}

impl Default for PeerKeyboard {
    fn default() -> Self {
        Self { on: true, modifiers: ModifierMap::default() }
    }
}

impl PeerKeyboard {
    /// The mapping actually used between these two OSes. Automatic swaps Ctrl
    /// and Cmd when exactly one side is a Mac, so copy and paste stay under
    /// the same finger.
    pub fn effective(&self, local: Os, peer: Os) -> ModifierMap {
        match self.modifiers {
            ModifierMap::Automatic if (local == Os::MacOs) != (peer == Os::MacOs) => ModifierMap::SwapCtrlMeta,
            ModifierMap::Automatic => ModifierMap::AsIs,
            other => other,
        }
    }

    /// Applies the modifier mapping to a key about to be sent.
    pub fn remap(&self, key: Key, local: Os, peer: Os) -> Key {
        let meta = [Key::LEFT_META, Key::RIGHT_META];
        let pair = match self.effective(local, peer) {
            ModifierMap::SwapCtrlMeta => [Key::LEFT_CTRL, Key::RIGHT_CTRL],
            ModifierMap::SwapAltMeta => [Key::LEFT_ALT, Key::RIGHT_ALT],
            ModifierMap::AsIs | ModifierMap::Automatic => return key,
        };
        for i in 0..2 {
            if key == pair[i] {
                return meta[i];
            }
            if key == meta[i] {
                return pair[i];
            }
        }
        key
    }
}

// ── Displays ─────────────────────────────────────────────────────────

choice!(
    /// How the two devices' screens are arranged and where the pointer crosses.
    DisplayLayout {
        Exact => "Exact",
        Proportional => "Proportional",
    } default Exact
);

// ── Clipboard ────────────────────────────────────────────────────────

// ── Per device ───────────────────────────────────────────────────────────

/// What goes one way between this device and another.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Direction {
    pub mouse: bool,
    pub keyboard: bool,
    pub clipboard: bool,
}

impl Default for Direction {
    fn default() -> Self {
        Self { mouse: true, keyboard: true, clipboard: true }
    }
}

/// Rules for one paired device, in each direction.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct DeviceRules {
    /// Its name, as last seen.
    pub name: String,
    /// From this device to it.
    pub send: Direction,
    /// From it to this device.
    pub receive: Direction,
}

/// Which input a rule is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Mouse,
    Keyboard,
    Clipboard,
}

impl Direction {
    pub fn get(&self, channel: Channel) -> bool {
        match channel {
            Channel::Mouse => self.mouse,
            Channel::Keyboard => self.keyboard,
            Channel::Clipboard => self.clipboard,
        }
    }

    pub fn set(&mut self, channel: Channel, on: bool) {
        match channel {
            Channel::Mouse => self.mouse = on,
            Channel::Keyboard => self.keyboard = on,
            Channel::Clipboard => self.clipboard = on,
        }
    }
}

/// Sharing the clipboard with the other device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ClipboardSettings {
    /// Copy on one device, paste on the other.
    pub enabled: bool,
    /// Before per-device settings: moved into each device's [`PeerClipboard`].
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub images: bool,
    #[serde(skip_serializing)]
    #[doc(hidden)]
    pub max_mb: u32,
}

/// Sharing the clipboard with one other device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct PeerClipboard {
    /// Share the clipboard with this device at all (both directions).
    pub on: bool,
    /// Also share copied images.
    pub images: bool,
    /// Larger copies aren't shared, in megabytes.
    pub max_mb: u32,
}

impl Default for PeerClipboard {
    fn default() -> Self {
        Self { on: true, images: true, max_mb: 32 }
    }
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self { enabled: true, images: true, max_mb: 32 }
    }
}

// ── Appearance ───────────────────────────────────────────────────────────

choice!(
    Appearance {
        System => "Match system",
        Light => "Light",
        Dark => "Dark",
    } default System
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_swaps_only_between_mac_and_pc() {
        let k = PeerKeyboard::default();
        assert_eq!(k.remap(Key::LEFT_CTRL, Os::Linux, Os::MacOs), Key::LEFT_META);
        assert_eq!(k.remap(Key::RIGHT_META, Os::MacOs, Os::Windows), Key::RIGHT_CTRL);
        assert_eq!(k.remap(Key::LEFT_CTRL, Os::Linux, Os::Windows), Key::LEFT_CTRL);
        assert_eq!(k.remap(Key::LEFT_CTRL, Os::MacOs, Os::MacOs), Key::LEFT_CTRL);
    }

    #[test]
    fn explicit_swaps() {
        let k = PeerKeyboard { modifiers: ModifierMap::SwapAltMeta, ..Default::default() };
        assert_eq!(k.remap(Key::LEFT_ALT, Os::Linux, Os::Linux), Key::LEFT_META);
        assert_eq!(k.remap(Key::LEFT_META, Os::Linux, Os::Linux), Key::LEFT_ALT);
        assert_eq!(k.remap(Key::LEFT_CTRL, Os::Linux, Os::Linux), Key::LEFT_CTRL);
        let k = PeerKeyboard { modifiers: ModifierMap::AsIs, ..Default::default() };
        assert_eq!(k.remap(Key::LEFT_CTRL, Os::Linux, Os::MacOs), Key::LEFT_CTRL);
    }
}
