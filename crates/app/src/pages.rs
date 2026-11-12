use dextr_core::{
    edge::Link,
    Mode,
    settings::{Appearance, Codec, FrameRate, ModifierMap, ReturnShortcut, StreamQuality},
};
use dextr_proto::{Os, Rect, Side};
use iced::{
    Alignment, Background, Border, Element, Font, Length, Theme,
    widget::{
        button, canvas, column, container, pick_list, row, scrollable, slider, space, text, text_input, toggler,
    },
};

use crate::{Access, App, Danger, Message, Page, Setting, arrangement::Arrangement, autostart, os_name};

// ── Building blocks ──────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Tone {
    Good,
    Active,
    Warn,
    Bad,
    Neutral,
}

fn chip<'a>(label: impl text::IntoFragment<'a>, tone: Tone) -> Element<'a, Message> {
    container(text(label).size(12))
        .padding([3, 10])
        .style(move |theme: &Theme| {
            let p = theme.extended_palette();
            let pair = match tone {
                Tone::Good => p.success.weak,
                Tone::Active => p.primary.weak,
                Tone::Warn => p.warning.weak,
                Tone::Bad => p.danger.weak,
                Tone::Neutral => p.background.strong,
            };
            container::Style {
                background: Some(Background::Color(pair.color)),
                text_color: Some(pair.text),
                border: Border { radius: 12.0.into(), ..Border::default() },
                ..container::Style::default()
            }
        })
        .into()
}

const SEMIBOLD: Font = Font { weight: iced::font::Weight::Semibold, ..Font::DEFAULT };

fn card<'a>(title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(column![text(title).size(15).font(SEMIBOLD), body.into()].spacing(14))
        .padding(18)
        .width(Length::Fill)
        .style(|theme: &Theme| {
            let p = theme.extended_palette();
            container::Style {
                background: Some(Background::Color(p.background.weaker.color)),
                border: Border { radius: 10.0.into(), width: 1.0, color: p.background.weak.color },
                ..container::Style::default()
            }
        })
        .into()
}

fn muted<'a>(content: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(content).size(13).style(|theme: &Theme| text::Style {
        color: Some(theme.extended_palette().background.strong.text.scale_alpha(0.75)),
    })
}

fn heading<'a>(title: &'a str, subtitle: &'a str) -> Element<'a, Message> {
    column![text(title).size(24), muted(subtitle)].spacing(4).into()
}

fn keycap(label: &str) -> Element<'_, Message> {
    container(text(label).size(13).font(Font::MONOSPACE))
        .padding([3, 9])
        .style(container::bordered_box)
        .into()
}

fn field<'a>(label: &'a str, input: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![text(label).size(13), input.into()].spacing(6).into()
}

/// A labeled setting row: title and description on the left, control on the right.
fn setting<'a>(title: &'a str, description: &'a str, control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    row![
        column![text(title).size(14), muted(description).size(12)].spacing(2).width(Length::Fill),
        control.into(),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

fn switch<'a>(title: &'a str, description: &'a str, on: bool, f: fn(bool) -> Setting) -> Element<'a, Message> {
    setting(title, description, toggler(on).on_toggle(move |v| Message::Set(f(v))))
}

fn choose<'a, T>(title: &'a str, description: &'a str, options: &'static [T], value: T, f: fn(T) -> Setting) -> Element<'a, Message>
where
    T: ToString + PartialEq + Clone + 'static,
{
    setting(title, description, pick_list(options, Some(value), move |v| Message::Set(f(v))).width(230))
}

#[allow(clippy::too_many_arguments)]
fn slide<'a>(
    title: &'a str,
    description: &'a str,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    value: f32,
    shown: String,
    f: fn(f32) -> Setting,
) -> Element<'a, Message> {
    setting(
        title,
        description,
        row![
            slider(range, value, move |v| Message::Set(f(v))).step(step).on_release(Message::Save).width(200),
            text(shown).size(13).font(Font::MONOSPACE).width(70),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
}

fn settings_list<'a>(items: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    column(items).spacing(18).into()
}

fn banner<'a>(content: impl Into<Element<'a, Message>>, tone: Tone) -> container::Container<'a, Message> {
    container(content).padding([10, 14]).width(Length::Fill).style(move |theme: &Theme| {
        let p = theme.extended_palette();
        let pair = match tone {
            Tone::Bad => p.danger.weak,
            Tone::Active => p.primary.weak,
            _ => p.warning.weak,
        };
        container::Style {
            background: Some(Background::Color(pair.color)),
            text_color: Some(pair.text),
            border: Border { radius: 8.0.into(), ..Border::default() },
            ..container::Style::default()
        }
    })
}

fn mode_chip(app: &App) -> Element<'_, Message> {
    match (app.mode, &app.peer) {
        (None, _) => chip("Starting", Tone::Neutral),
        (Some(Mode::Controlling), _) => chip("Controlling", Tone::Active),
        (Some(Mode::Controlled), _) => chip("Controlled", Tone::Active),
        (Some(Mode::Local), Some(_)) if app.paused => chip("Paused", Tone::Warn),
        (Some(Mode::Local), Some(_)) => chip("Connected", Tone::Good),
        (Some(Mode::Local), None) if app.connecting => chip("Connecting", Tone::Neutral),
        (Some(Mode::Local), None) if app.reconnect_in.is_some() => chip("Reconnecting", Tone::Warn),
        (Some(Mode::Local), None) => chip("Offline", Tone::Neutral),
    }
}

fn connect_button(app: &App) -> button::Button<'_, Message> {
    if app.peer.is_some() {
        button(text("Disconnect").center().width(Length::Fill)).style(button::secondary).on_press(Message::Disconnect)
    } else if app.connecting {
        button(text("Connecting…").center().width(Length::Fill)).style(button::secondary)
    } else if app.reconnect_in.is_some() {
        button(text("Stop reconnecting").center().width(Length::Fill)).style(button::secondary).on_press(Message::Disconnect)
    } else {
        button(text("Connect").center().width(Length::Fill))
            .style(button::primary)
            .on_press_maybe(app.saved.is_some().then_some(Message::Connect))
    }
}

/// A destructive action with a second click to confirm.
fn danger_row<'a>(app: &App, title: &'a str, description: &'a str, action: &'a str, danger: Danger) -> Element<'a, Message> {
    let control: Element<'a, Message> = if app.confirm == Some(danger) {
        row![
            button("Cancel").style(button::secondary).on_press(Message::Cancel),
            button(text(format!("Yes, {}", action.to_lowercase()))).style(button::danger).on_press(Message::Confirm(danger)),
        ]
        .spacing(8)
        .into()
    } else {
        button(action).style(button::secondary).on_press(Message::Arm(danger)).into()
    };
    setting(title, description, control)
}

fn side_word(side: Side) -> String {
    side.to_string().to_lowercase()
}

// ── Layout ───────────────────────────────────────────────────────────────

pub fn view(app: &App) -> Element<'_, Message> {
    let page = match app.page {
        Page::Overview => overview(app),
        Page::Displays => displays(app),
        Page::Pairing => pairing(app),
        Page::Mouse => mouse(app),
        Page::Keyboard => keyboard(app),
        Page::Windows => windows(app),
        Page::Activity => activity(app),
        Page::Settings => settings(app),
    };

    let mut content = column![].spacing(16).max_width(780);
    if let Some(notice) = &app.notice {
        content = content.push(
            banner(
                row![text(notice).size(13).width(Length::Fill), button("Dismiss").style(button::text).on_press(Message::DismissNotice)]
                    .spacing(12)
                    .align_y(Alignment::Center),
                Tone::Warn,
            ),
        );
    }
    content = content.push(page);

    let body = scrollable(container(content).padding(28).width(Length::Fill).center_x(Length::Fill))
        .height(Length::Fill)
        .width(Length::Fill);

    row![sidebar(app), body].into()
}

fn sidebar(app: &App) -> Element<'_, Message> {
    let nav = column(Page::ALL.iter().map(|&p| {
        let label: Element<'_, Message> = if p == Page::Windows {
            row![text(p.label()).size(14), space::horizontal(), text("Preview").size(11)].align_y(Alignment::Center).into()
        } else {
            text(p.label()).size(14).into()
        };
        button(label)
            .width(Length::Fill)
            .padding([8, 12])
            .style(if app.page == p { button::primary } else { button::text })
            .on_press(Message::Navigate(p))
            .into()
    }))
    .spacing(3);

    let peer_line: Element<'_, Message> = match (&app.peer, app.reconnect_in) {
        (Some(p), _) => column![text(&p.name).size(13), muted(format!("{} · {}", os_name(p.os), p.address)).size(12)]
            .spacing(2)
            .into(),
        (None, Some(secs)) => muted(format!("Retrying in {secs} s…")).size(12).into(),
        (None, None) => muted("No device connected").size(12).into(),
    };

    container(
        column![
            row![text("dextr").size(22), space::horizontal(), mode_chip(app)].align_y(Alignment::Center),
            space().height(8),
            nav,
            space::vertical(),
            peer_line,
            connect_button(app),
        ]
        .spacing(10),
    )
    .padding(16)
    .width(230)
    .height(Length::Fill)
    .style(|theme: &Theme| {
        let p = theme.extended_palette();
        container::Style {
            background: Some(Background::Color(p.background.weakest.color)),
            border: Border { width: 1.0, color: p.background.weak.color, ..Border::default() },
            ..container::Style::default()
        }
    })
    .into()
}

fn arrangement_canvas(app: &App, interactive: bool, height: f32) -> Element<'_, Message> {
    let peer_label: &str = app.peer.as_ref().map(|p| p.name.as_str()).unwrap_or_else(|| {
        app.saved.as_ref().and_then(|c| c.last_peer_name.as_deref()).unwrap_or("Other device")
    });
    let local_label = app.saved.as_ref().map(|c| c.device_name.as_str()).unwrap_or("This device");
    let on_change: fn(Link) -> Message = |l| Message::Set(Setting::Arrange(l));
    canvas(Arrangement {
        local: &app.local_screens,
        peer: app.peer_screens(),
        link: app.config().link(),
        local_label,
        peer_label,
        on_change: interactive.then_some(on_change),
    })
    .width(Length::Fill)
    .height(height)
    .into()
}

// ── Overview ─────────────────────────────────────────────────────────────

fn overview(app: &App) -> Element<'_, Message> {
    let side = side_word(app.config().peer_side);
    let (status, detail): (String, String) = match (&app.peer, app.mode) {
        (Some(p), Some(Mode::Controlling)) => (format!("Controlling {}", p.name), "Your mouse and keyboard are on the other device.".into()),
        (Some(p), Some(Mode::Controlled)) => (format!("{} is controlling this device", p.name), "Its mouse and keyboard are on this screen.".into()),
        (Some(p), _) => (format!("Connected to {}", p.name), format!("{} at {}. Push the pointer past the {side} edge to switch.", os_name(p.os), p.address)),
        (None, _) if app.connecting => ("Connecting…".into(), format!("Reaching {}", app.form.address)),
        (None, _) if app.reconnect_in.is_some() => (
            "Connection lost".into(),
            format!("Trying {} again in {} s.", app.form.address, app.reconnect_in.unwrap_or_default()),
        ),
        (None, _) if app.form.address.is_empty() || app.form.fingerprint.is_empty() => {
            ("Not paired yet".into(), "Pair with your other device on the Pairing page.".into())
        }
        (None, _) => ("Not connected".into(), format!("Ready to connect to {}.", app.form.address)),
    };

    let connection = card(
        "Connection",
        column![
            row![column![text(status).size(20), muted(detail)].spacing(4).width(Length::Fill), connect_button(app).width(160)]
                .spacing(16)
                .align_y(Alignment::Center),
            setting(
                "Share this mouse and keyboard",
                "Turn off to keep the pointer on this device for a while.",
                toggler(!app.paused).on_toggle(|on| Message::Paused(!on)),
            ),
        ]
        .spacing(16),
    );

    let arrangement = card(
        "Arrangement",
        column![
            arrangement_canvas(app, false, 170.0),
            row![
                muted(format!("{} is on the {side} of this device.", app.peer_name())).width(Length::Fill),
                button("Arrange displays").style(button::secondary).on_press(Message::Navigate(Page::Displays)),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(10),
    );

    column![heading("Overview", "Share one mouse and keyboard between your computers."), connection, arrangement, permissions_card(app)]
        .spacing(16)
        .into()
}

fn permissions_card(app: &App) -> Element<'_, Message> {
    fn line<'a>(what: &'a str, why: &'a str, access: &'a Access) -> Element<'a, Message> {
        let (status, detail): (Element<'a, Message>, Option<&'a str>) = match access {
            Access::Pending => (chip("Waiting", Tone::Neutral), Some("Answer the system dialog to continue.")),
            Access::Granted => (chip("Allowed", Tone::Good), None),
            Access::Denied(reason) => (chip("Unavailable", Tone::Bad), Some(reason.as_str())),
        };
        let mut info = column![text(what).size(14), muted(why).size(12)].spacing(2).width(Length::Fill);
        if let Some(d) = detail {
            info = info.push(muted(d).size(12));
        }
        row![info, status].spacing(12).align_y(Alignment::Center).into()
    }
    let any_denied = matches!(app.capture, Access::Denied(_)) || matches!(app.inject, Access::Denied(_));
    let mut body = column![
        line("Control the other device", "Lets this mouse and keyboard move to the other screen.", &app.capture),
        line("Be controlled", "Lets the other device's mouse and keyboard work here.", &app.inject),
    ]
    .spacing(14);
    if any_denied {
        body = body.push(row![space::horizontal(), button("Ask again").style(button::secondary).on_press(Message::RetryPermissions)]);
    }
    card("Permissions", body)
}

// ── Displays ─────────────────────────────────────────────────────────────

fn screen_list<'a>(screens: &[Rect], empty: &'a str) -> Element<'a, Message> {
    if screens.is_empty() {
        return muted(empty).into();
    }
    column(screens.iter().enumerate().map(|(i, s)| {
        row![
            text(format!("Screen {}", i + 1)).size(13).width(80),
            text(format!("{} × {}", s.w, s.h)).size(13).font(Font::MONOSPACE).width(Length::Fill),
            muted(format!("at {}, {}", s.x, s.y)).size(12),
        ]
        .into()
    }))
    .spacing(6)
    .into()
}

fn displays(app: &App) -> Element<'_, Message> {
    let c = app.config();
    let details = row![
        column![
            text(app.saved.as_ref().map(|c| c.device_name.as_str()).unwrap_or("This device")).size(14),
            screen_list(&app.local_screens, "Screens appear once input permissions are allowed."),
        ]
        .spacing(8)
        .width(Length::Fill),
        column![text(app.peer_name()).size(14), screen_list(app.peer_screens().unwrap_or_default(), "Not connected.")]
            .spacing(8)
            .width(Length::Fill),
    ]
    .spacing(24);

    let switching = card(
        "Switching",
        settings_list(vec![
            slide(
                "Dead corners",
                "Crossing is ignored this close to the ends of the edge, so corners stay usable.",
                0.0..=120.0,
                4.0,
                c.switching.dead_corners as f32,
                format!("{} px", c.switching.dead_corners),
                |v| Setting::DeadCorners(v as u32),
            ),
            slide(
                "Switch delay",
                "Push against the edge this long before switching. Moving away cancels.",
                0.0..=1000.0,
                50.0,
                c.switching.delay_ms as f32,
                if c.switching.delay_ms == 0 { "Off".into() } else { format!("{} ms", c.switching.delay_ms) },
                |v| Setting::SwitchDelay(v as u32),
            ),
        ]),
    );

    column![
        heading("Displays", "Drag the other device's screens to where it sits on your desk."),
        card(
            "Arrangement",
            column![
                arrangement_canvas(app, true, 340.0),
                muted("Drag the screens to any side and slide them along it. The pointer crosses only where the edges touch (the green line), at the same spot on both screens. They snap to centered or aligned edges when close."),
            ]
            .spacing(14),
        ),
        switching,
        card("Screens", details),
    ]
    .spacing(16)
    .into()
}

// ── Pairing ──────────────────────────────────────────────────────────────

fn pairing(app: &App) -> Element<'_, Message> {
    let this_device = card(
        "This device",
        column![
            muted("On the other device, paste this into Pairing → Fingerprint."),
            container(text(if app.fingerprint.is_empty() { "Loading…" } else { &app.fingerprint }).font(Font::MONOSPACE).size(15))
                .padding(12)
                .width(Length::Fill)
                .style(container::bordered_box),
            row![space::horizontal(), button("Copy fingerprint").style(button::secondary).on_press(Message::CopyFingerprint)],
        ]
        .spacing(10),
    );

    let other = card(
        "Other device",
        column![
            field(
                "Address",
                text_input("192.168.1.20, or name.local, or host:port", &app.form.address)
                    .on_input(Message::Address)
                    .on_submit(Message::Connect)
                    .padding(8),
            ),
            field(
                "Fingerprint",
                row![
                    text_input("Paste the other device's fingerprint", &app.form.fingerprint)
                        .on_input(Message::PeerFingerprint)
                        .on_submit(Message::Save)
                        .font(Font::MONOSPACE)
                        .padding(8),
                    button("Paste").style(button::secondary).on_press(Message::PasteFingerprint),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ),
            row![
                space::horizontal(),
                button("Save").style(button::secondary).on_press_maybe(app.unsaved().then_some(Message::Save)),
                connect_button(app).width(160),
            ]
            .spacing(8),
        ]
        .spacing(14),
    );

    let steps = card(
        "How pairing works",
        column![
            muted("1. Open dextr on both devices."),
            muted("2. On each device, paste the other device's fingerprint. Only that device can connect."),
            muted("3. On one of them, enter the other's address and press Connect. UDP port 47800 must be allowed through the firewall."),
        ]
        .spacing(6),
    );

    column![heading("Pairing", "Connect to your other device."), this_device, other, steps].spacing(16).into()
}

// ── Mouse ────────────────────────────────────────────────────────────────

fn mouse(app: &App) -> Element<'_, Message> {
    let m = &app.config().mouse;
    let pointer = card(
        "Pointer",
        settings_list(vec![
            slide(
                "Pointer speed",
                "How fast the pointer moves on the other device, relative to this one.",
                0.25..=3.0,
                0.05,
                m.speed,
                format!("{:.2}×", m.speed),
                Setting::PointerSpeed,
            ),
            switch("Swap left and right buttons", "For left-handed use on the other device.", m.swap_buttons, Setting::SwapButtons),
        ]),
    );

    let scrolling = card(
        "Scrolling",
        settings_list(vec![
            slide(
                "Scroll speed",
                "How far each scroll moves on the other device.",
                0.25..=3.0,
                0.05,
                m.scroll_speed,
                format!("{:.2}×", m.scroll_speed),
                Setting::ScrollSpeed,
            ),
            switch(
                "Natural scrolling",
                "Content follows your fingers, like a phone. Reverses the direction on the other device.",
                m.natural_scroll,
                Setting::NaturalScroll,
            ),
        ]),
    );

    let sharing = card(
        "Sharing",
        settings_list(vec![setting(
            "Share this mouse",
            "When off, the pointer stays on this device.",
            toggler(!app.paused).on_toggle(|on| Message::Paused(!on)),
        )]),
    );

    column![heading("Mouse", "How this mouse behaves on the other device."), pointer, scrolling, sharing].spacing(16).into()
}

// ── Keyboard ─────────────────────────────────────────────────────────────

fn keyboard(app: &App) -> Element<'_, Message> {
    let k = &app.config().keyboard;
    let explain = match app.peer_os() {
        Some(peer) => {
            let effective = k.effective(Os::current(), peer);
            format!("With {} ({}) this means: {}.", app.peer_name(), os_name(peer), effective.to_string().to_lowercase())
        }
        None => "Automatic swaps Ctrl and Cmd when exactly one of the two devices is a Mac, so copy and paste stay under the same finger.".into(),
    };

    let modifiers = card(
        "Modifier keys",
        column![
            choose("Mapping", "How Ctrl, Alt and Cmd/Super are sent.", ModifierMap::ALL, k.modifiers, Setting::Modifiers),
            muted(explain).size(12),
        ]
        .spacing(10),
    );

    let shortcut_keys: Element<'_, Message> = match k.return_shortcut {
        ReturnShortcut::CtrlAltEsc => row![keycap("Ctrl"), text("+").size(13), keycap("Alt"), text("+").size(13), keycap("Esc")]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        ReturnShortcut::ScrollLock => keycap("Scroll Lock"),
        ReturnShortcut::Pause => keycap("Pause"),
    };
    let shortcuts = card(
        "Shortcuts",
        column![
            choose(
                "Bring the pointer back",
                "Works from the other device at any time. This key isn't sent to it.",
                ReturnShortcut::ALL,
                k.return_shortcut,
                Setting::ReturnShortcut,
            ),
            row![space::horizontal(), shortcut_keys],
        ]
        .spacing(10),
    );

    let layout = card(
        "Layout",
        column![
            muted("Keys are sent by their position, like a USB keyboard. The other device types with its own keyboard layout."),
            muted("Every key and button you're holding is released whenever the pointer switches, so nothing gets stuck."),
        ]
        .spacing(6),
    );

    column![heading("Keyboard", "How this keyboard behaves on the other device."), modifiers, shortcuts, layout]
        .spacing(16)
        .into()
}

// ── Windows ──────────────────────────────────────────────────────────────

fn windows(app: &App) -> Element<'_, Message> {
    let w = &app.config().windows;
    let preview = banner(
        column![
            text("Window sharing is in development").size(14).font(SEMIBOLD),
            text("These settings are saved now and take effect when window sharing arrives in a future update.").size(13),
        ]
        .spacing(4),
        Tone::Active,
    );

    let dragging = card(
        "Dragging windows",
        settings_list(vec![
            switch(
                "Drag windows to the other device",
                "Move a window past the shared edge to show it on the other screen.",
                w.enabled,
                Setting::WindowsEnabled,
            ),
            switch(
                "Accept windows from the other device",
                "Let the other device show its windows here.",
                w.accept_incoming,
                Setting::WindowsAcceptIncoming,
            ),
            switch(
                "Bring windows back when disconnected",
                "Windows return to their own device if the connection drops.",
                w.return_on_disconnect,
                Setting::ReturnOnDisconnect,
            ),
        ]),
    );

    let native = card(
        "Native look",
        settings_list(vec![
            switch(
                "Use the other device's window frame",
                "Shown windows get that device's own title bar, shadow and taskbar entry.",
                w.native_frames,
                Setting::NativeFrames,
            ),
            switch("Match light and dark mode", "Switch the app to the other device's appearance.", w.match_theme, Setting::MatchTheme),
            switch("Play sound where the window is", "Audio follows the window to the other device.", w.follow_audio, Setting::FollowAudio),
        ]),
    );

    let quality = card(
        "Quality",
        settings_list(vec![
            choose(
                "Picture",
                "Sharpest text sends still content without loss. Low bandwidth suits slow Wi-Fi.",
                StreamQuality::ALL,
                w.quality,
                Setting::Quality,
            ),
            choose("Video codec", "Used for moving content. Automatic picks the best one both devices support in hardware.", Codec::ALL, w.codec, Setting::Codec),
            choose("Frame rate", "Upper limit for moving content.", FrameRate::ALL, w.frame_rate, Setting::FrameRate),
        ]),
    );

    column![heading("Windows", "Move app windows between your devices."), preview, dragging, native, quality]
        .spacing(16)
        .into()
}

// ── Activity ─────────────────────────────────────────────────────────────

fn activity(app: &App) -> Element<'_, Message> {
    let list: Element<'_, Message> = if app.log.is_empty() {
        muted("Nothing yet.").into()
    } else {
        column(app.log.iter().map(|l| text(l).size(13).into())).spacing(8).into()
    };
    column![
        row![
            heading("Activity", "Connections, permissions and errors, newest first."),
            space::horizontal(),
            button("Clear").style(button::secondary).on_press_maybe((!app.log.is_empty()).then_some(Message::ClearLog)),
        ]
        .align_y(Alignment::Center),
        card("Log", list),
    ]
    .spacing(16)
    .into()
}

// ── Settings ─────────────────────────────────────────────────────────────

fn settings(app: &App) -> Element<'_, Message> {
    let c = app.config();
    let port_changed = app.saved.as_ref().is_some_and(|c| c.port.to_string() != app.form.port.trim());

    let device = card(
        "Device",
        column![
            field("Name", text_input("Shown on the other device", &app.form.name).on_input(Message::Name).on_submit(Message::Save).padding(8)),
            field("Port (UDP)", text_input("47800", &app.form.port).on_input(Message::Port).on_submit(Message::Save).padding(8).width(140)),
            muted(match (app.listening, port_changed) {
                (_, true) => "The new port takes effect after restarting dextr.".to_string(),
                (Some(port), false) => format!("Listening on UDP port {port}."),
                (None, false) => "Not listening. Another app may be using this port.".to_string(),
            }),
            row![space::horizontal(), button("Save").style(button::primary).on_press_maybe(app.unsaved().then_some(Message::Save))],
        ]
        .spacing(14),
    );

    let startup_desc = if autostart::supported() { "Start dextr automatically when you log in." } else { "Not available on this OS yet." };
    let startup = card(
        "Startup and connection",
        settings_list(vec![
            setting(
                "Launch at login",
                startup_desc,
                toggler(app.launch_at_login).on_toggle_maybe(autostart::supported().then_some(Message::LaunchAtLogin)),
            ),
            switch("Connect automatically", "Connect to the paired device when dextr starts.", c.connection.auto_connect, Setting::AutoConnect),
            switch(
                "Reconnect automatically",
                "Keep trying after the connection drops, waiting up to 30 seconds between tries.",
                c.connection.auto_reconnect,
                Setting::AutoReconnect,
            ),
        ]),
    );

    let privacy = card(
        "Privacy and security",
        settings_list(vec![
            switch(
                "Allow being controlled",
                "Let the paired device's mouse and keyboard work on this device.",
                c.connection.allow_control,
                Setting::AllowControl,
            ),
            switch(
                "Accept incoming connections",
                "When off, only this device can start a connection.",
                c.connection.allow_incoming,
                Setting::AllowIncoming,
            ),
            muted("Connections are encrypted with QUIC (TLS 1.3). Only the device whose fingerprint you paired can connect.").size(12).into(),
        ]),
    );

    let appearance = card(
        "Appearance",
        settings_list(vec![choose("Theme", "Colors used by this window.", Appearance::ALL, c.appearance, Setting::Appearance)]),
    );

    let config_dir = dextr_core::config::dir().map(|d| d.display().to_string()).unwrap_or_else(|e| e.to_string());
    let files = card(
        "Files",
        column![
            muted("Settings and this device's certificate are stored here. Keep key.der private."),
            row![
                text(config_dir).font(Font::MONOSPACE).size(13).width(Length::Fill),
                button("Open folder").style(button::secondary).on_press(Message::OpenConfigFolder),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        ]
        .spacing(10),
    );

    let reset = card(
        "Reset",
        settings_list(vec![
            danger_row(app, "Forget paired device", "Disconnect and remove its address and fingerprint.", "Forget", Danger::ForgetPeer),
            danger_row(
                app,
                "New fingerprint",
                if app.restart_needed {
                    "Restart dextr to create it, then pair again."
                } else {
                    "Create a new identity for this device. You'll need to pair again."
                },
                "Reset fingerprint",
                Danger::NewFingerprint,
            ),
            danger_row(app, "Reset all settings", "Restore defaults. Keeps pairing, name and port.", "Reset", Danger::ResetSettings),
        ]),
    );

    let about = card(
        "About",
        column![text(format!("dextr {}", env!("CARGO_PKG_VERSION"))).size(14), muted("Mouse, keyboard and window sharing between your computers.")]
            .spacing(6),
    );

    column![heading("Settings", "General options for this device."), device, startup, privacy, appearance, files, reset, about]
        .spacing(16)
        .into()
}
