//! The dextr wire protocol.
//!
//! Every peer speaks exactly this protocol, whatever its OS, so any device can
//! control any other. Messages are encoded with `postcard` and framed with a
//! little-endian `u32` length prefix on reliable QUIC streams. Pointer motion
//! is also sent on its own as an unreliable QUIC datagram (one message per
//! datagram, no length prefix): motion is absolute, so a lost datagram is
//! simply superseded by the next one.

pub mod geometry;
pub mod keys;

pub use geometry::{Rect, Side};
pub use keys::Key;

use serde::{Deserialize, Serialize};

/// Bumped on every incompatible change to [`Message`].
pub const PROTOCOL_VERSION: u16 = 5;

/// ALPN identifier negotiated on the QUIC connection.
pub const ALPN: &[u8] = b"dextr/1";

/// Default UDP port.
pub const DEFAULT_PORT: u16 = 47800;

/// UDP port for finding other dextr devices on the local network.
pub const DISCOVERY_PORT: u16 = 47801;

/// Largest frame accepted on a reliable stream.
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
    Other,
}

impl Os {
    pub const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            Os::Other
        }
    }
}

/// First message each side sends after the connection is established.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Hello {
    pub protocol: u16,
    pub name: String,
    pub os: Os,
    /// The sender's screens, in its own logical-pixel coordinate space.
    /// This is the space the *other* side uses for [`Input::Motion`].
    pub displays: Vec<Rect>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    /// Anything else, as a Linux `BTN_*` code (the de-facto common numbering).
    Other(u16),
}

/// One input event, already translated into OS-independent terms.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum Input {
    /// Absolute pointer position in the *receiver's* logical coordinates.
    Motion { x: f32, y: f32 },
    /// Carries the pointer position so a click lands in the right place even
    /// if the preceding motion datagram was lost or reordered.
    Button { button: Button, pressed: bool, x: f32, y: f32 },
    /// Smooth scrolling, in logical pixels.
    Scroll { dx: f32, dy: f32 },
    /// Wheel detents, in units of 1/120 of a detent (120 = one click).
    ScrollDiscrete { dx: i32, dy: i32 },
    /// End of a smooth-scroll sequence (lets the receiver start kinetic scrolling).
    ScrollStop,
    Key { key: Key, pressed: bool },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Message {
    Hello(Hello),
    /// The sender's pointer crossed its edge: the receiver now owns the
    /// pointer, starting at this position (receiver coordinates).
    Enter { x: f32, y: f32 },
    /// Control goes back to the sender. The receiver releases held input.
    Leave,
    Input(Input),
    /// Release every key and button the sender pressed on the receiver.
    ReleaseAll,
    /// The sender's screen layout changed.
    Displays(Vec<Rect>),
    /// Asks to pair (from the device that dialed), or answers with the
    /// responder's own details. Sent instead of [`Message::Hello`] by a device
    /// that isn't paired yet.
    PairRequest { name: String, os: Os },
    /// The sender's user confirmed that both devices show the same code.
    PairAccept,
    /// The sender's user declined or cancelled.
    PairReject,
    /// The shared arrangement of all devices' screens (by certificate
    /// fingerprint). Receivers keep it if `changed_at` (Unix ms) is newer than
    /// their own.
    Layout {
        proportional: bool,
        /// Exact: each device's top-left in the shared space.
        origins: Vec<([u8; 32], i32, i32)>,
        /// Proportional: devices left to right.
        row: Vec<[u8; 32]>,
        changed_at: u64,
    },
}

/// Clipboard contents, sent on a unidirectional stream of its own (it can
/// be large) as one length-prefixed [`Clipboard`] message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Clipboard {
    Text(String),
    /// A PNG image.
    Image { png: Vec<u8> },
}

/// Largest clipboard message accepted.
pub const MAX_CLIPBOARD: usize = 256 * 1024 * 1024;

impl Clipboard {
    pub fn to_frame(&self) -> Result<Vec<u8>, CodecError> {
        let payload = postcard::to_stdvec(self)?;
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

/// Broadcast on [`DISCOVERY_PORT`] every few seconds so devices on the same
/// network can list each other. Carries nothing secret: pairing still needs
/// both users to confirm a code.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Beacon {
    pub magic: [u8; 5],
    pub protocol: u16,
    pub name: String,
    pub os: Os,
    /// The QUIC port the sender listens on.
    pub port: u16,
    /// SHA-256 of the sender's certificate.
    pub fingerprint: [u8; 32],
}

impl Beacon {
    pub const MAGIC: [u8; 5] = *b"dextr";

    pub fn to_bytes(&self) -> Result<Vec<u8>, CodecError> {
        Ok(postcard::to_stdvec(self)?)
    }

    /// Decodes a beacon, ignoring anything that isn't one.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let b: Beacon = postcard::from_bytes(bytes).ok()?;
        (b.magic == Self::MAGIC).then_some(b)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME}-byte limit")]
    TooLarge(usize),
    #[error(transparent)]
    Postcard(#[from] postcard::Error),
}

impl Message {
    /// Encodes as a bare payload (used for datagrams).
    pub fn to_bytes(&self) -> Result<Vec<u8>, CodecError> {
        Ok(postcard::to_stdvec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(postcard::from_bytes(bytes)?)
    }

    /// Encodes with a `u32` LE length prefix (used on reliable streams).
    pub fn to_frame(&self) -> Result<Vec<u8>, CodecError> {
        let payload = self.to_bytes()?;
        if payload.len() > MAX_FRAME {
            return Err(CodecError::TooLarge(payload.len()));
        }
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let msg = Message::Input(Input::Button {
            button: Button::Left,
            pressed: true,
            x: 10.5,
            y: 20.0,
        });
        let frame = msg.to_frame().unwrap();
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        assert_eq!(Message::from_bytes(&frame[4..]).unwrap(), msg);
    }

    #[test]
    fn motion_datagram_is_small() {
        let msg = Message::Input(Input::Motion { x: 1919.0, y: 1079.0 });
        assert!(msg.to_bytes().unwrap().len() <= 12);
    }
}
