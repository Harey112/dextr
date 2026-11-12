# dextr

Share one mouse and keyboard between computers, in any direction, written in Rust.

## Layout

| Crate | Job |
|---|---|
| `crates/proto` | Wire protocol: messages, HID key codes, screen geometry. Same on every OS. |
| `crates/platform` | OS backends behind one interface: input **capture** and input **injection**. |
| `crates/core` | Engine: QUIC transport, pairing by certificate fingerprint, pointer hand-off. |
| `crates/app` | iced desktop app (`dextr` binary): settings, pairing, status. |

## Status

| | Linux (Wayland) | macOS | Windows |
|---|---|---|---|
| Control the peer (capture) | ✅ InputCapture portal + libei | ⏳ CGEventTap | ⏳ Raw Input + hook |
| Be controlled (inject) | ✅ RemoteDesktop portal + libei | ⏳ CGEventPost | ⏳ SendInput |

Linux needs a desktop whose portal has InputCapture: KDE Plasma 6.1+ or GNOME 45+.
No `input` group or `/dev/input` access is used.

## Run

```sh
cargo run --release
```

On first start the desktop asks twice: once to allow remote input, once to allow input capture.

## Pair two devices

1. Run dextr on both.
2. On each, paste the **other** device's fingerprint into *Peer → Fingerprint*, and pick which side the peer is on.
3. On one of them, enter the other's address and press **Connect**. UDP port 47800 must be open.

Push the pointer off the chosen edge to move to the peer. **Ctrl+Alt+Esc** always brings it back.

Settings and the device certificate live in `~/.config/dextr/`.

## How hand-off works

The controlling side keeps a virtual pointer in the peer's coordinates and sends absolute positions:
motion as unreliable QUIC datagrams (a lost one is replaced by the next), and clicks, keys and scrolling
on a reliable stream. Clicks carry the pointer position, so they land correctly even if motion was dropped.
Keys travel as USB HID usages, so the receiving computer applies its own keyboard layout.
