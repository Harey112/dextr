# dextr

Share one mouse, keyboard and clipboard between computers, in any direction, written in Rust.

## Layout

| Crate | Job |
|---|---|
| `crates/proto` | Wire protocol: messages, HID key codes, screen geometry. Same on every OS. |
| `crates/platform` | OS backends behind one interface: input **capture**, input **injection**, the **clipboard**. |
| `crates/core` | Engine: QUIC transport, discovery, pairing by code, pointer hand-off, clipboard sync. |
| `crates/app` | iced desktop app (`dextr` binary): settings, pairing, status. |

## Status

| | Linux (Wayland) | macOS | Windows |
|---|---|---|---|
| Control the peer (capture) | ✅ InputCapture portal + libei | 🧪 Quartz event tap | ⏳ Raw Input + hook |
| Be controlled (inject) | ✅ RemoteDesktop portal + libei | 🧪 CGEventPost | ⏳ SendInput |
| Launch at login | ✅ autostart entry | 🧪 LaunchAgent | ⏳ |
| Tray icon, runs in the background | ✅ StatusNotifierItem | 🧪 menu bar (tray-icon) | 🧪 notification area |
| Clipboard (text, images) | ✅ Wayland data-control (KDE Plasma, wlroots) | 🧪 NSPasteboard | ⏳ |

✅ works · 🧪 written, compiles for macOS, not yet tested on a Mac · ⏳ not started

Linux needs a desktop whose portal has InputCapture: KDE Plasma 6.1+ or GNOME 45+.
No `input` group or `/dev/input` access is used.

macOS media keys (volume, play) are not forwarded to a Mac yet.

## Fonts

The app uses [Poppins](https://github.com/itfoundry/Poppins) (SIL Open Font License, `assets/fonts/Poppins-OFL.txt`)
and the [Lucide](https://lucide.dev) icon font (ISC, `assets/fonts/Lucide-LICENSE.txt`), built into the binary.

## Icon

`assets/icon.svg` is the source. `assets/icons/` holds PNGs from 16 to 1024 px and
`assets/dextr.icns` is the macOS icon. On Linux, dextr installs its icon and a launcher entry for the
current user on start (`~/.local/share/icons` and `~/.local/share/applications/dev.dextr.dextr.desktop`),
so the taskbar and app menu show it. On macOS it sets its Dock icon itself.

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

## Pair devices

1. Open dextr on both devices, on the same network.
2. On one of them, go to **Pairing**. The other device appears under **Nearby devices**: press **Pair**.
   (Not listed? Type one of the addresses shown on the other device's Pairing page under
   **Pair by address**. This also works over Tailscale or a VPN.)
3. Both screens show the same 6-digit code. If they match, press **Codes match** on both.

That's all: they connect now, and from then on by themselves, even after one gets a new IP address.
Pair as many devices as you like; each one connects to all the others. Arrange the screens on the
**Displays** page: every device keeps the same layout (the newest change wins). Push the pointer off
a shared edge to move to the device there, and on to the next one from there (A → B → C).
**Ctrl+Alt+Esc** always brings it back.

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

## Running in the background

Closing the window keeps dextr running; its tray icon reopens it, shows the connection, pauses
sharing, or quits. **Launch at login** starts it in the tray without a window (`dextr --hidden`).

* **Linux:** the StatusNotifierItem tray protocol, in pure Rust (no GTK or system libraries). Shown
  by KDE Plasma, Xfce, Cinnamon, MATE, LXQt, Budgie and wlroots bars; on GNOME with the AppIndicator
  extension (built in on Ubuntu and Fedora). With no tray at all, closing the window quits as before.
* **macOS / Windows:** the native menu bar or notification area.

## Displays: Exact or Proportional

* **Exact** (default): real sizes, like the system display settings. Place each device on any
  side of another and slide it along; the pointer crosses only where the edges touch, at the same pixel.
* **Proportional:** every device's screens in one flat row, all the same height. The whole edge maps to
  the whole edge: leaving 30% of the way down one enters 30% of the way down the next, whatever the
  sizes, so there are no dead zones. The edge is the outermost screen on that side.

The arrangement, including the mode, is the same on every device. Newly paired devices are placed
automatically until you move them.

## What goes where

The **Mouse**, **Keyboard** and **Clipboard** pages each have a main switch, and a card per paired
device with its own switch, a switch per direction (this device → it, and it → this device), and its
own options: pointer and scroll speed, natural scrolling and swapped buttons; modifier keys; images
and the largest copy. All the switches on the way must be on. Turning the mouse off keeps the pointer (and so the
keyboard) on this device; turning the keyboard off keeps keys from being sent while the pointer is
over there.

## Clipboard

Copy on one device, paste on any other the rules allow: text and images (as PNG), in both directions. Each copy is
sent once, right away, over the encrypted connection, on a QUIC stream of its own so a large image
never holds up the mouse or keyboard. Images are compared by their pixels, so a picture re-encoded
by the other OS isn't sent back. Turn it off, leave images out, or cap the size on the **Clipboard**
page.

* **Linux:** works in the background through the Wayland data-control protocol (KDE Plasma, wlroots
  desktops). Text is checked a few times a second; images about once a second, compared as the raw
  PNG bytes and decoded only when they change.
* **macOS:** the pasteboard's change counter is checked a few times a second, and the contents are
  read only when it changes.

## How hand-off works

The controlling side keeps a virtual pointer in the peer's coordinates and sends absolute positions:
motion as unreliable QUIC datagrams (a lost one is replaced by the next), and clicks, keys and scrolling
on a reliable stream. Clicks carry the pointer position, so they land correctly even if motion was dropped.
Keys travel as USB HID usages, so the receiving computer applies its own keyboard layout.
