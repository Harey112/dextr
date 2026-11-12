# dextr

Share one mouse and keyboard between computers, in any direction, written in Rust.

## Layout

| Crate | Job |
|---|---|
| `crates/proto` | Wire protocol: messages, HID key codes, screen geometry. Same on every OS. |
| `crates/platform` | OS backends behind one interface: input **capture** and input **injection**. |
| `crates/core` | Engine: QUIC transport, discovery, pairing by code, pointer hand-off. |
| `crates/app` | iced desktop app (`dextr` binary): settings, pairing, status. |

## Status

| | Linux (Wayland) | macOS | Windows |
|---|---|---|---|
| Control the peer (capture) | ✅ InputCapture portal + libei | 🧪 Quartz event tap | ⏳ Raw Input + hook |
| Be controlled (inject) | ✅ RemoteDesktop portal + libei | 🧪 CGEventPost | ⏳ SendInput |
| Launch at login | ✅ autostart entry | 🧪 LaunchAgent | ⏳ |
| Share a window (send) | ✅ ScreenCast portal + PipeWire; input on KDE Plasma | 🧪 Quartz window capture | ⏳ |
| Show a shared window (receive) | ✅ | ✅ (same iced code) | ⏳ |

✅ works · 🧪 written, compiles for macOS, not yet tested on a Mac · ⏳ not started

Linux needs a desktop whose portal has InputCapture: KDE Plasma 6.1+ or GNOME 45+.
No `input` group or `/dev/input` access is used.

macOS media keys (volume, play) are not forwarded to a Mac yet.

## Run

### Linux

```sh
cargo run --release
```

On first start the desktop asks twice: once to allow remote input, once to allow input capture.

### macOS

Needs macOS 11 or newer, the Xcode command line tools and Rust:

```sh
xcode-select --install                                          # if not installed yet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  # if Rust isn't installed yet
git clone https://github.com/Harey112/dextr.git
cd dextr
cargo run --release
```

macOS then asks for two permissions. Allow them in **System Settings → Privacy & Security**:

* **Accessibility**: to move the pointer and type when the other device controls this Mac, and to
  take over the mouse and keyboard when this Mac controls the other device.
* **Input Monitoring**: to read the mouse and keyboard while controlling the other device.

When dextr is started from a terminal, macOS asks for these for the **terminal app** (Terminal,
iTerm, …), so that is the entry to switch on. Afterwards press **Ask again** on the Overview page, or
quit and restart dextr. If macOS asks whether dextr may accept incoming network connections, allow it.

## Pair two devices

1. Open dextr on both devices, on the same network.
2. On one of them, go to **Pairing**. The other device appears under **Nearby devices**: press **Pair**.
   (Not listed? Type one of the addresses shown on the other device's Pairing page under
   **Pair by address**. This also works over Tailscale or a VPN.)
3. Both screens show the same 6-digit code. If they match, press **Codes match** on both.

That's all: they connect now, and from then on by themselves, even after one gets a new IP address.
Arrange the screens on the **Displays** page; both devices keep the same layout (the newest change wins). Push the pointer off the shared edge to move to the
other device. **Ctrl+Alt+Esc** always brings it back.

dextr uses UDP port **47800** for the connection and **47801** to find devices. On Linux with a
firewall (Fedora, for example), allow them:

```sh
sudo firewall-cmd --permanent --add-port=47800-47801/udp && sudo firewall-cmd --reload
```

### How pairing is secured

Each device has its own certificate. While pairing, the two devices connect without trusting each
other yet and derive the code from **both** certificates. A device in the middle can't present
either certificate, so the two screens would show different codes. Once confirmed, each device
remembers the other's certificate fingerprint and accepts only that device from then on.
Discovery broadcasts carry only a name, OS, port and fingerprint.

Settings and the device certificate live in `~/.config/dextr/` on Linux and
`~/Library/Application Support/dev.dextr.dextr/` on macOS.

## Window sharing

On the **Windows** page, press **Choose a window…** (Linux: the desktop's own picker) or pick one
from the list (macOS). It opens on the other device as a real native window, with that device's own
title bar and taskbar entry. Mouse, scrolling and keyboard in it go back to the original window.

* **Pixel for pixel.** The window is captured at its real resolution (Retina too), and the receiving
  window is sized so one of its pixels is one screen pixel, drawn without smoothing. Nothing is
  scaled or video-compressed, so text is exactly as sharp as on the original.
* **Only what changes is sent.** Frames are compared in 64×64 tiles and only changed tiles go over the
  network, LZ4-compressed. A still window costs nothing. If the network is slower than the window
  changes, frames are skipped, never queued, so it never lags behind.
* **Linux:** no extra packages are needed (PipeWire is loaded at runtime). On KDE Plasma, a small KWin
  script tells dextr where the window is, so input reaches it; on other desktops it's view-only.
* **macOS:** needs **Screen Recording** permission (System Settings → Privacy & Security).

The shared window stays on its own device too. Dragging a window across the edge isn't supported yet.

## How hand-off works

The controlling side keeps a virtual pointer in the peer's coordinates and sends absolute positions:
motion as unreliable QUIC datagrams (a lost one is replaced by the next), and clicks, keys and scrolling
on a reliable stream. Clicks carry the pointer position, so they land correctly even if motion was dropped.
Keys travel as USB HID usages, so the receiving computer applies its own keyboard layout.
