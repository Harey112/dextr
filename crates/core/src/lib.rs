//! The dextr engine: pairing, QUIC transport and pointer hand-off.
//!
//! The engine is OS-independent. It drives the platform backends through
//! `dextr-platform` handles and speaks `dextr-proto` to the peer, so the same
//! code runs in every direction (Linux ↔ macOS ↔ Windows).
//!
//! UIs talk to it through two channels: [`Command`]s in, [`Event`]s out.

pub mod config;
pub mod discovery;
pub mod edge;
pub mod identity;
pub mod settings;
mod engine;
mod tls;

pub use config::Config;
pub use discovery::LocalAddress;
pub use engine::{Command, Event, Mode, Nearby, PairingState, spawn};
