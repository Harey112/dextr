//! Physical key identity, independent of keyboard layout and OS.
//!
//! Keys travel as USB HID usages. The receiver applies its *own* layout, so a
//! German keyboard driving a US-layout machine types what the receiver's
//! layout says, exactly like plugging a USB keyboard into it.
//!
//! Each backend owns one translation table between its native codes and HID.
//! The Linux evdev table lives here because evdev is the reference numbering
//! most other tools (libei, uinput, Wayland) also use.

use serde::{Deserialize, Serialize};

/// A HID usage: `page << 16 | id`. Page 0x07 is the keyboard page, page 0x0C
/// the consumer page (media and volume keys).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub u32);

pub const PAGE_KEYBOARD: u32 = 0x07;
pub const PAGE_CONSUMER: u32 = 0x0C;

impl Key {
    pub const fn keyboard(id: u16) -> Self {
        Key(PAGE_KEYBOARD << 16 | id as u32)
    }

    pub const fn consumer(id: u16) -> Self {
        Key(PAGE_CONSUMER << 16 | id as u32)
    }

    pub const fn page(self) -> u32 {
        self.0 >> 16
    }

    pub const fn id(self) -> u16 {
        self.0 as u16
    }

    pub const LEFT_CTRL: Key = Key::keyboard(0xE0);
    pub const LEFT_SHIFT: Key = Key::keyboard(0xE1);
    pub const LEFT_ALT: Key = Key::keyboard(0xE2);
    pub const LEFT_META: Key = Key::keyboard(0xE3);
    pub const RIGHT_CTRL: Key = Key::keyboard(0xE4);
    pub const RIGHT_SHIFT: Key = Key::keyboard(0xE5);
    pub const RIGHT_ALT: Key = Key::keyboard(0xE6);
    pub const RIGHT_META: Key = Key::keyboard(0xE7);
    pub const ESCAPE: Key = Key::keyboard(0x29);

    pub fn is_ctrl(self) -> bool {
        self == Key::LEFT_CTRL || self == Key::RIGHT_CTRL
    }

    pub fn is_alt(self) -> bool {
        self == Key::LEFT_ALT || self == Key::RIGHT_ALT
    }
}

/// Linux evdev `KEY_*` code ↔ HID usage. evdev → HID is unique; for HID →
/// evdev the first row wins.
const EVDEV_HID: &[(u16, Key)] = &[
    // Letters
    (30, Key::keyboard(0x04)), // A
    (48, Key::keyboard(0x05)), // B
    (46, Key::keyboard(0x06)), // C
    (32, Key::keyboard(0x07)), // D
    (18, Key::keyboard(0x08)), // E
    (33, Key::keyboard(0x09)), // F
    (34, Key::keyboard(0x0A)), // G
    (35, Key::keyboard(0x0B)), // H
    (23, Key::keyboard(0x0C)), // I
    (36, Key::keyboard(0x0D)), // J
    (37, Key::keyboard(0x0E)), // K
    (38, Key::keyboard(0x0F)), // L
    (50, Key::keyboard(0x10)), // M
    (49, Key::keyboard(0x11)), // N
    (24, Key::keyboard(0x12)), // O
    (25, Key::keyboard(0x13)), // P
    (16, Key::keyboard(0x14)), // Q
    (19, Key::keyboard(0x15)), // R
    (31, Key::keyboard(0x16)), // S
    (20, Key::keyboard(0x17)), // T
    (22, Key::keyboard(0x18)), // U
    (47, Key::keyboard(0x19)), // V
    (17, Key::keyboard(0x1A)), // W
    (45, Key::keyboard(0x1B)), // X
    (21, Key::keyboard(0x1C)), // Y
    (44, Key::keyboard(0x1D)), // Z
    // Digit row
    (2, Key::keyboard(0x1E)),
    (3, Key::keyboard(0x1F)),
    (4, Key::keyboard(0x20)),
    (5, Key::keyboard(0x21)),
    (6, Key::keyboard(0x22)),
    (7, Key::keyboard(0x23)),
    (8, Key::keyboard(0x24)),
    (9, Key::keyboard(0x25)),
    (10, Key::keyboard(0x26)),
    (11, Key::keyboard(0x27)),
    // Editing and punctuation
    (28, Key::keyboard(0x28)), // Enter
    (1, Key::keyboard(0x29)),  // Esc
    (14, Key::keyboard(0x2A)), // Backspace
    (15, Key::keyboard(0x2B)), // Tab
    (57, Key::keyboard(0x2C)), // Space
    (12, Key::keyboard(0x2D)), // -
    (13, Key::keyboard(0x2E)), // =
    (26, Key::keyboard(0x2F)), // [
    (27, Key::keyboard(0x30)), // ]
    (43, Key::keyboard(0x31)), // backslash (also ISO #, 0x32)
    (39, Key::keyboard(0x33)), // ;
    (40, Key::keyboard(0x34)), // '
    (41, Key::keyboard(0x35)), // `
    (51, Key::keyboard(0x36)), // ,
    (52, Key::keyboard(0x37)), // .
    (53, Key::keyboard(0x38)), // /
    (58, Key::keyboard(0x39)), // Caps Lock
    // F1–F12
    (59, Key::keyboard(0x3A)),
    (60, Key::keyboard(0x3B)),
    (61, Key::keyboard(0x3C)),
    (62, Key::keyboard(0x3D)),
    (63, Key::keyboard(0x3E)),
    (64, Key::keyboard(0x3F)),
    (65, Key::keyboard(0x40)),
    (66, Key::keyboard(0x41)),
    (67, Key::keyboard(0x42)),
    (68, Key::keyboard(0x43)),
    (87, Key::keyboard(0x44)),
    (88, Key::keyboard(0x45)),
    // Navigation block
    (99, Key::keyboard(0x46)),  // Print Screen / SysRq
    (70, Key::keyboard(0x47)),  // Scroll Lock
    (119, Key::keyboard(0x48)), // Pause
    (110, Key::keyboard(0x49)), // Insert
    (102, Key::keyboard(0x4A)), // Home
    (104, Key::keyboard(0x4B)), // Page Up
    (111, Key::keyboard(0x4C)), // Delete
    (107, Key::keyboard(0x4D)), // End
    (109, Key::keyboard(0x4E)), // Page Down
    (106, Key::keyboard(0x4F)), // Right
    (105, Key::keyboard(0x50)), // Left
    (108, Key::keyboard(0x51)), // Down
    (103, Key::keyboard(0x52)), // Up
    // Keypad
    (69, Key::keyboard(0x53)), // Num Lock
    (98, Key::keyboard(0x54)), // KP /
    (55, Key::keyboard(0x55)), // KP *
    (74, Key::keyboard(0x56)), // KP -
    (78, Key::keyboard(0x57)), // KP +
    (96, Key::keyboard(0x58)), // KP Enter
    (79, Key::keyboard(0x59)),
    (80, Key::keyboard(0x5A)),
    (81, Key::keyboard(0x5B)),
    (75, Key::keyboard(0x5C)),
    (76, Key::keyboard(0x5D)),
    (77, Key::keyboard(0x5E)),
    (71, Key::keyboard(0x5F)),
    (72, Key::keyboard(0x60)),
    (73, Key::keyboard(0x61)),
    (82, Key::keyboard(0x62)),  // KP 0
    (83, Key::keyboard(0x63)),  // KP .
    (86, Key::keyboard(0x64)),  // ISO extra key (102nd)
    (127, Key::keyboard(0x65)), // Menu / Compose
    (116, Key::keyboard(0x66)), // Power
    (117, Key::keyboard(0x67)), // KP =
    // F13–F24
    (183, Key::keyboard(0x68)),
    (184, Key::keyboard(0x69)),
    (185, Key::keyboard(0x6A)),
    (186, Key::keyboard(0x6B)),
    (187, Key::keyboard(0x6C)),
    (188, Key::keyboard(0x6D)),
    (189, Key::keyboard(0x6E)),
    (190, Key::keyboard(0x6F)),
    (191, Key::keyboard(0x70)),
    (192, Key::keyboard(0x71)),
    (193, Key::keyboard(0x72)),
    (194, Key::keyboard(0x73)),
    // International / CJK
    (121, Key::keyboard(0x85)), // KP ,
    (89, Key::keyboard(0x87)),  // Ro
    (93, Key::keyboard(0x88)),  // Katakana/Hiragana
    (124, Key::keyboard(0x89)), // Yen
    (92, Key::keyboard(0x8A)),  // Henkan
    (94, Key::keyboard(0x8B)),  // Muhenkan
    (122, Key::keyboard(0x90)), // Hangeul
    (123, Key::keyboard(0x91)), // Hanja
    (85, Key::keyboard(0x94)),  // Zenkaku/Hankaku
    // Modifiers
    (29, Key::LEFT_CTRL),
    (42, Key::LEFT_SHIFT),
    (56, Key::LEFT_ALT),
    (125, Key::LEFT_META),
    (97, Key::RIGHT_CTRL),
    (54, Key::RIGHT_SHIFT),
    (100, Key::RIGHT_ALT),
    (126, Key::RIGHT_META),
    // Media and volume (consumer page)
    (113, Key::consumer(0xE2)), // Mute
    (114, Key::consumer(0xEA)), // Volume Down
    (115, Key::consumer(0xE9)), // Volume Up
    (163, Key::consumer(0xB5)), // Next
    (165, Key::consumer(0xB6)), // Previous
    (166, Key::consumer(0xB7)), // Stop
    (164, Key::consumer(0xCD)), // Play/Pause
    (224, Key::consumer(0x70)), // Brightness Down
    (225, Key::consumer(0x6F)), // Brightness Up
];

pub fn from_evdev(code: u32) -> Option<Key> {
    let code = u16::try_from(code).ok()?;
    EVDEV_HID.iter().find(|(e, _)| *e == code).map(|(_, k)| *k)
}

pub fn to_evdev(key: Key) -> Option<u32> {
    if key == Key::keyboard(0x32) {
        return Some(43); // ISO # shares evdev KEY_BACKSLASH
    }
    EVDEV_HID.iter().find(|(_, k)| *k == key).map(|(e, _)| *e as u32)
}

/// macOS virtual keycode (`kVK_*`) ↔ HID usage. A bijection.
const MAC_HID: &[(u16, u16)] = &[
    // Letters
    (0x00, 0x04), (0x0B, 0x05), (0x08, 0x06), (0x02, 0x07), (0x0E, 0x08), (0x03, 0x09),
    (0x05, 0x0A), (0x04, 0x0B), (0x22, 0x0C), (0x26, 0x0D), (0x28, 0x0E), (0x25, 0x0F),
    (0x2E, 0x10), (0x2D, 0x11), (0x1F, 0x12), (0x23, 0x13), (0x0C, 0x14), (0x0F, 0x15),
    (0x01, 0x16), (0x11, 0x17), (0x20, 0x18), (0x09, 0x19), (0x0D, 0x1A), (0x07, 0x1B),
    (0x10, 0x1C), (0x06, 0x1D),
    // Digits 1–9, 0
    (0x12, 0x1E), (0x13, 0x1F), (0x14, 0x20), (0x15, 0x21), (0x17, 0x22),
    (0x16, 0x23), (0x1A, 0x24), (0x1C, 0x25), (0x19, 0x26), (0x1D, 0x27),
    // Return, Escape, Delete (backspace), Tab, Space
    (0x24, 0x28), (0x35, 0x29), (0x33, 0x2A), (0x30, 0x2B), (0x31, 0x2C),
    // - = [ ] \ ; ' ` , . /
    (0x1B, 0x2D), (0x18, 0x2E), (0x21, 0x2F), (0x1E, 0x30), (0x2A, 0x31), (0x29, 0x33),
    (0x27, 0x34), (0x32, 0x35), (0x2B, 0x36), (0x2F, 0x37), (0x2C, 0x38),
    // Caps Lock
    (0x39, 0x39),
    // F1–F12
    (0x7A, 0x3A), (0x78, 0x3B), (0x63, 0x3C), (0x76, 0x3D), (0x60, 0x3E), (0x61, 0x3F),
    (0x62, 0x40), (0x64, 0x41), (0x65, 0x42), (0x6D, 0x43), (0x67, 0x44), (0x6F, 0x45),
    // Help/Insert, Home, Page Up, Forward Delete, End, Page Down
    (0x72, 0x49), (0x73, 0x4A), (0x74, 0x4B), (0x75, 0x4C), (0x77, 0x4D), (0x79, 0x4E),
    // Right, Left, Down, Up
    (0x7C, 0x4F), (0x7B, 0x50), (0x7D, 0x51), (0x7E, 0x52),
    // Keypad: Clear/Num Lock, / * - + Enter 1–9 0 .
    (0x47, 0x53), (0x4B, 0x54), (0x43, 0x55), (0x4E, 0x56), (0x45, 0x57), (0x4C, 0x58),
    (0x53, 0x59), (0x54, 0x5A), (0x55, 0x5B), (0x56, 0x5C), (0x57, 0x5D), (0x58, 0x5E),
    (0x59, 0x5F), (0x5B, 0x60), (0x5C, 0x61), (0x52, 0x62), (0x41, 0x63),
    // ISO §, context menu, keypad =
    (0x0A, 0x64), (0x6E, 0x65), (0x51, 0x67),
    // F13–F20
    (0x69, 0x68), (0x6B, 0x69), (0x71, 0x6A), (0x6A, 0x6B), (0x40, 0x6C), (0x4F, 0x6D),
    (0x50, 0x6E), (0x5A, 0x6F),
    // Mute, Volume Up, Volume Down
    (0x4A, 0x7F), (0x48, 0x80), (0x49, 0x81),
    // JIS: keypad comma, Ro, Yen, Kana, Eisu
    (0x5F, 0x85), (0x5E, 0x87), (0x5D, 0x89), (0x68, 0x90), (0x66, 0x91),
    // Modifiers: Control, Shift, Option, Command (left, then right)
    (0x3B, 0xE0), (0x38, 0xE1), (0x3A, 0xE2), (0x37, 0xE3),
    (0x3E, 0xE4), (0x3C, 0xE5), (0x3D, 0xE6), (0x36, 0xE7),
];

pub fn from_mac(code: u16) -> Option<Key> {
    MAC_HID.iter().find(|(m, _)| *m == code).map(|(_, h)| Key::keyboard(*h))
}

pub fn to_mac(key: Key) -> Option<u16> {
    if key.page() != PAGE_KEYBOARD {
        return None;
    }
    if key == Key::keyboard(0x32) {
        return Some(0x2A); // ISO # shares the backslash key
    }
    MAC_HID.iter().find(|(_, h)| *h == key.id()).map(|(m, _)| *m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_table_is_a_bijection() {
        for (i, (e, k)) in EVDEV_HID.iter().enumerate() {
            assert!(
                EVDEV_HID[..i].iter().all(|(e2, k2)| e2 != e && k2 != k),
                "duplicate entry for evdev {e} / {k:?}"
            );
            assert_eq!(from_evdev(*e as u32), Some(*k));
            assert_eq!(to_evdev(*k), Some(*e as u32));
        }
    }

    #[test]
    fn mac_table_is_a_bijection() {
        for (i, (m, h)) in MAC_HID.iter().enumerate() {
            assert!(MAC_HID[..i].iter().all(|(m2, h2)| m2 != m && h2 != h), "duplicate entry for mac {m:#x} / {h:#x}");
            assert_eq!(from_mac(*m), Some(Key::keyboard(*h)));
            assert_eq!(to_mac(Key::keyboard(*h)), Some(*m));
        }
        assert_eq!(to_mac(Key::consumer(0xE9)), None);
    }
}
