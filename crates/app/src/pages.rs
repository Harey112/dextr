use dextr_core::{
    config::PeerEntry,
    Mode, PairingState,
    settings::{Appearance, Channel, DisplayLayout, ModifierMap, ReturnShortcut},
};
use dextr_proto::{DEFAULT_PORT, Os, Rect};
use iced::{
    Alignment, Background, Border, Color, Element, Font, Length, Theme,
    widget::{
        button, canvas, column, container, pick_list, row, scrollable, slider, space, text, text_input, toggler, tooltip,
    },
};

use crate::{Access, App, Danger, Message, Page, PeerChange, Setting, Update, arrangement::Arrangement, autostart, os_name, update};

// ── Type and icons ───────────────────────────────────────────────────────

const MEDIUM: Font = Font { weight: iced::font::Weight::Medium, ..Font::with_name("Poppins") };
const SEMIBOLD: Font = Font { weight: iced::font::Weight::Semibold, ..Font::with_name("Poppins") };
const ICONS: Font = Font::with_name("lucide");

/// Lucide icon code points.
mod ic {
    pub const OVERVIEW: char = '\u{e1c1}';
    pub const DISPLAYS: char = '\u{e11d}';
    pub const PAIRING: char = '\u{e102}';
    pub const MOUSE: char = '\u{e28e}';
    pub const KEYBOARD: char = '\u{e284}';
    pub const CLIPBOARD: char = '\u{e085}';
    pub const ACTIVITY: char = '\u{e038}';
    pub const SETTINGS: char = '\u{e154}';
    pub const HELP: char = '\u{e082}';
    pub const PLUG: char = '\u{e37f}';
    pub const UNPLUG: char = '\u{e45d}';
    pub const COPY: char = '\u{e09e}';
    pub const TRASH: char = '\u{e18e}';
    pub const REFRESH: char = '\u{e145}';
    pub const LAPTOP: char = '\u{e1cd}';
    pub const SHIELD: char = '\u{e1ff}';
    pub const PALETTE: char = '\u{e1dd}';
    pub const FOLDER: char = '\u{e247}';
    pub const ROTATE: char = '\u{e148}';
    pub const RADAR: char = '\u{e497}';
    pub const NETWORK: char = '\u{e125}';
    pub const TO: char = '\u{e049}';
    pub const SLIDERS: char = '\u{e29a}';
    pub const ROCKET: char = '\u{e286}';
    pub const INFO: char = '\u{e0f9}';
    pub const ZAP: char = '\u{e1b4}';
    pub const FINGERPRINT: char = '\u{e2cb}';
    pub const CHECK: char = '\u{e06c}';
}

fn icon<'a>(code: char) -> text::Text<'a> {
    text(code.to_string()).font(ICONS).size(16).line_height(1.0)
}

/// A label with an icon in front.
fn with_icon<'a>(code: char, label: impl text::IntoFragment<'a>) -> Element<'a, Message> {
    row![icon(code), text(label)].spacing(8).align_y(Alignment::Center).into()
}

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
    container(text(label).size(12).font(MEDIUM))
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

fn muted<'a>(content: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(content).size(13).style(|theme: &Theme| text::Style {
        color: Some(theme.extended_palette().background.strong.text.scale_alpha(0.75)),
    })
}

/// A help circle: hover (or focus) to read `description`.
fn help<'a>(description: impl text::IntoFragment<'a>) -> Element<'a, Message> {
    let mark = icon(ic::HELP).size(14).style(|theme: &Theme| text::Style {
        color: Some(theme.extended_palette().background.strong.text.scale_alpha(0.55)),
    });
    tooltip(
        mark,
        container(text(description).size(12)).padding([8, 12]).max_width(300).style(|theme: &Theme| {
            let p = theme.extended_palette();
            container::Style {
                background: Some(Background::Color(p.background.base.color)),
                text_color: Some(p.background.base.text),
                border: Border { radius: 8.0.into(), width: 1.0, color: p.background.strong.color },
                shadow: iced::Shadow { color: Color::BLACK.scale_alpha(0.18), offset: iced::Vector::new(0.0, 4.0), blur_radius: 14.0 },
                ..container::Style::default()
            }
        }),
        tooltip::Position::Top,
    )
    .gap(6)
    .into()
}

/// A card: icon and title on top, optional help, then the body.
fn card<'a>(code: char, title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    card_with(code, title, None, body)
}

fn card_help<'a>(code: char, title: &'a str, help_text: impl text::IntoFragment<'a>, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    card_with(code, title, Some(help(help_text)), body)
}

fn card_with<'a>(code: char, title: &'a str, help_mark: Option<Element<'a, Message>>, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let mut header = row![
        icon(code).size(17).style(|theme: &Theme| text::Style { color: Some(theme.extended_palette().primary.base.color) }),
        text(title).size(15).font(SEMIBOLD),
    ]
    .spacing(9)
    .align_y(Alignment::Center);
    if let Some(h) = help_mark {
        header = header.push(h);
    }
    container(column![header, body.into()].spacing(16))
        .padding(20)
        .width(Length::Fill)
        .style(|theme: &Theme| {
            let p = theme.extended_palette();
            container::Style {
                background: Some(Background::Color(p.background.weaker.color)),
                border: Border { radius: 12.0.into(), width: 1.0, color: p.background.weak.color },
                ..container::Style::default()
            }
        })
        .into()
}

/// Page title with an optional master switch on the right.
fn heading<'a>(code: char, title: &'a str, subtitle: &'a str, master: Option<Element<'a, Message>>) -> Element<'a, Message> {
    let titles = column![
        row![icon(code).size(24), text(title).size(26).font(SEMIBOLD)].spacing(12).align_y(Alignment::Center),
        muted(subtitle).size(14),
    ]
    .spacing(4)
    .width(Length::Fill);
    let mut r = row![titles].spacing(16).align_y(Alignment::Center);
    if let Some(m) = master {
        r = r.push(m);
    }
    r.into()
}

/// The big on/off switch of a page.
fn master<'a>(label: &'a str, on: bool, f: fn(bool) -> Setting) -> Element<'a, Message> {
    container(
        row![text(label).size(14).font(MEDIUM), toggler(on).size(22).on_toggle(move |v| Message::Set(f(v)))]
            .spacing(12)
            .align_y(Alignment::Center),
    )
    .padding([10, 16])
    .style(move |theme: &Theme| {
        let p = theme.extended_palette();
        let pair = if on { p.success.weak } else { p.background.strong };
        container::Style {
            background: Some(Background::Color(pair.color)),
            text_color: Some(pair.text),
            border: Border { radius: 22.0.into(), ..Border::default() },
            ..container::Style::default()
        }
    })
    .into()
}

fn keycap(label: &str) -> Element<'_, Message> {
    container(text(label).size(13).font(Font::MONOSPACE))
        .padding([3, 9])
        .style(container::bordered_box)
        .into()
}

fn field<'a>(label: &'a str, input: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![text(label).size(13).font(MEDIUM), input.into()].spacing(6).into()
}

/// A setting row: name (and a help circle if there's more to say) on the
/// left, control on the right.
fn setting<'a>(title: &'a str, description: &'a str, control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let mut name = row![text(title).size(14)].spacing(6).align_y(Alignment::Center).width(Length::Fill);
    if !description.is_empty() {
        name = name.push(help(description));
    }
    row![
        name,
        control.into(),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

fn switch<'a>(title: &'a str, description: &'a str, on: bool, f: impl Fn(bool) -> Setting + 'a) -> Element<'a, Message> {
    setting(title, description, toggler(on).on_toggle(move |v| Message::Set(f(v))))
}

fn choose<'a, T>(title: &'a str, description: &'a str, options: &'static [T], value: T, f: impl Fn(T) -> Setting + 'a) -> Element<'a, Message>
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
    f: impl Fn(f32) -> Setting + 'a,
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
    container(content).padding([12, 16]).width(Length::Fill).style(move |theme: &Theme| {
        let p = theme.extended_palette();
        let pair = match tone {
            Tone::Bad => p.danger.weak,
            Tone::Active => p.primary.weak,
            _ => p.warning.weak,
        };
        container::Style {
            background: Some(Background::Color(pair.color)),
            text_color: Some(pair.text),
            border: Border { radius: 10.0.into(), ..Border::default() },
            ..container::Style::default()
        }
    })
}

fn mode_chip(app: &App) -> Element<'_, Message> {
    let connected = app.peers.len();
    match &app.mode {
        None => chip("Starting", Tone::Neutral),
        Some(Mode::Controlling(_)) => chip("Controlling", Tone::Active),
        Some(Mode::Controlled(_)) => chip("Controlled", Tone::Active),
        Some(Mode::Local) if connected > 0 && app.paused => chip("Paused", Tone::Warn),
        Some(Mode::Local) if connected > 0 => chip("Connected", Tone::Good),
        Some(Mode::Local) if !app.connecting.is_empty() => chip("Connecting", Tone::Neutral),
        Some(Mode::Local) if !app.reconnect_in.is_empty() => chip("Reconnecting", Tone::Warn),
        Some(Mode::Local) => chip("Offline", Tone::Neutral),
    }
}

/// Connect to or disconnect from every paired device.
fn connect_button(app: &App) -> button::Button<'_, Message> {
    let label = |code, s| container(with_icon(code, s)).center_x(Length::Fill);
    let busy = !app.peers.is_empty() || !app.connecting.is_empty() || !app.reconnect_in.is_empty();
    if busy {
        let text = if app.config().peers.len() > 1 { "Disconnect all" } else { "Disconnect" };
        button(label(ic::UNPLUG, text)).style(button::secondary).on_press(Message::Disconnect(None))
    } else {
        let text = if app.config().peers.len() > 1 { "Connect all" } else { "Connect" };
        button(label(ic::PLUG, text))
            .style(button::primary)
            .on_press_maybe((!app.config().peers.is_empty()).then_some(Message::Connect(None)))
    }
}

/// Connect or disconnect one device.
fn device_button<'a>(app: &'a App, id: &'a str) -> Element<'a, Message> {
    if app.peers.contains_key(id) {
        button(with_icon(ic::UNPLUG, "Disconnect")).style(button::secondary).on_press(Message::Disconnect(Some(id.to_owned()))).into()
    } else if app.connecting.contains(id) {
        button(with_icon(ic::PLUG, "Connecting…")).style(button::secondary).into()
    } else if app.reconnect_in.contains_key(id) {
        button(with_icon(ic::UNPLUG, "Stop")).style(button::secondary).on_press(Message::Disconnect(Some(id.to_owned()))).into()
    } else {
        button(with_icon(ic::PLUG, "Connect")).style(button::primary).on_press(Message::Connect(Some(id.to_owned()))).into()
    }
}

/// A device's connection state as a chip.
fn device_chip<'a>(app: &App, id: &str) -> Element<'a, Message> {
    if app.peers.contains_key(id) {
        chip("Connected", Tone::Good)
    } else if app.connecting.contains(id) {
        chip("Connecting", Tone::Neutral)
    } else if let Some(secs) = app.reconnect_in.get(id) {
        chip(format!("Retry in {secs} s"), Tone::Warn)
    } else {
        chip("Offline", Tone::Neutral)
    }
}

fn icon_button<'a>(code: char, label: &'a str, message: Message) -> button::Button<'a, Message> {
    button(with_icon(code, label)).style(button::secondary).on_press(message)
}

/// A destructive action with a second click to confirm.
fn danger_row<'a>(app: &App, title: &'a str, description: &'a str, action: &'a str, danger: Danger) -> Element<'a, Message> {
    let control: Element<'a, Message> = if app.confirm.as_ref() == Some(&danger) {
        row![
            button("Cancel").style(button::secondary).on_press(Message::Cancel),
            button(text(format!("Yes, {}", action.to_lowercase()))).style(button::danger).on_press(Message::Confirm(danger.clone())),
        ]
        .spacing(8)
        .into()
    } else {
        button(action).style(button::secondary).on_press(Message::Arm(danger)).into()
    };
    setting(title, description, control)
}

fn this_name(app: &App) -> &str {
    app.saved.as_ref().map(|c| c.device_name.as_str()).unwrap_or("This device")
}

// ── Layout ───────────────────────────────────────────────────────────────

pub fn view(app: &App) -> Element<'_, Message> {
    let page = match app.page {
        Page::Overview => overview(app),
        Page::Displays => displays(app),
        Page::Pairing => pairing(app),
        Page::Mouse => mouse(app),
        Page::Keyboard => keyboard(app),
        Page::Clipboard => clipboard(app),
        Page::Activity => activity(app),
        Page::Settings => settings(app),
    };

    let mut content = column![].spacing(18).max_width(800);
    if let Some(notice) = &app.notice {
        content = content.push(banner(
            row![text(notice).size(13).width(Length::Fill), button("Dismiss").style(button::text).on_press(Message::DismissNotice)]
                .spacing(12)
                .align_y(Alignment::Center),
            Tone::Warn,
        ));
    }
    content = content.push(page);

    let body = scrollable(container(content).padding([28, 32]).width(Length::Fill).center_x(Length::Fill))
        .height(Length::Fill)
        .width(Length::Fill);

    row![sidebar(app), body].into()
}

fn page_icon(p: Page) -> char {
    match p {
        Page::Overview => ic::OVERVIEW,
        Page::Displays => ic::DISPLAYS,
        Page::Pairing => ic::PAIRING,
        Page::Mouse => ic::MOUSE,
        Page::Keyboard => ic::KEYBOARD,
        Page::Clipboard => ic::CLIPBOARD,
        Page::Activity => ic::ACTIVITY,
        Page::Settings => ic::SETTINGS,
    }
}

fn sidebar(app: &App) -> Element<'_, Message> {
    let nav = column(Page::ALL.iter().map(|&p| {
        button(row![icon(page_icon(p)).size(17), text(p.label()).size(14)].spacing(12).align_y(Alignment::Center))
            .width(Length::Fill)
            .padding([9, 12])
            .style(if app.page == p { button::primary } else { button::text })
            .on_press(Message::Navigate(p))
            .into()
    }))
    .spacing(2);

    let peer_line: Element<'_, Message> = if app.peers.is_empty() {
        let paired = app.config().peers.len();
        muted(if paired == 0 { "No device paired".to_owned() } else { format!("0 of {paired} devices connected") }).size(12).into()
    } else {
        column(app.peers.values().map(|p| {
            row![icon(ic::LAPTOP).size(13), text(&p.name).size(13).font(MEDIUM), muted(os_name(p.os)).size(11)]
                .spacing(6)
                .align_y(Alignment::Center)
                .into()
        }))
        .spacing(4)
        .into()
    };

    container(
        column![
            row![text("dextr").size(24).font(SEMIBOLD), space::horizontal(), mode_chip(app)].align_y(Alignment::Center),
            space().height(10),
            nav,
            space::vertical(),
            peer_line,
            connect_button(app).width(Length::Fill),
        ]
        .spacing(10),
    )
    .padding(16)
    .width(236)
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
    let c = app.config();
    let mut names: std::collections::BTreeMap<String, String> = c.peers.keys().map(|id| (id.clone(), app.device_name(id))).collect();
    names.insert(app.fingerprint.clone(), this_name(app).to_owned());
    let on_change: fn(dextr_core::world::Arrangement) -> Message = |a| Message::Set(Setting::Arrange(a));
    canvas(Arrangement {
        me: &app.fingerprint,
        screens: app.all_screens(),
        names,
        connected: app.peers.keys().cloned().collect(),
        shared: &c.arrangement,
        proportional: c.layout == DisplayLayout::Proportional,
        new_side: c.new_device_side,
        on_change: interactive.then_some(on_change),
    })
    .width(Length::Fill)
    .height(height)
    .into()
}

// ── Per device ───────────────────────────────────────────────────────────

/// One card per paired device for `channel`: its own switch in the header,
/// then a switch per direction and the device's own `options`.
fn device_cards<'a>(
    app: &'a App,
    channel: Channel,
    options: impl Fn(&'a str, &'a PeerEntry) -> Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let devices = &app.config().peers;
    if devices.is_empty() {
        return card(ic::LAPTOP, "Devices", muted("Pair a device on the Pairing page to choose what's shared with it."));
    }
    let me = this_name(app);
    column(devices.iter().map(|(id, p)| {
        let name = if p.name.is_empty() { "Paired device" } else { p.name.as_str() };
        let on = match channel {
            Channel::Mouse => p.mouse.on,
            Channel::Keyboard => p.keyboard.on,
            Channel::Clipboard => p.clipboard.on,
        };
        let set = move |change: PeerChange| Message::Set(Setting::Peer { device: id.clone(), change });
        let header = row![
            icon(ic::LAPTOP).size(17).style(|theme: &Theme| text::Style { color: Some(theme.extended_palette().primary.base.color) }),
            text(name).size(15).font(SEMIBOLD),
            device_chip(app, id),
            space::horizontal(),
            toggler(on).size(22).on_toggle(move |v| set(PeerChange::On(channel, v))),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let mut body = column![header].spacing(16);
        if on {
            let direction = |from: &'a str, to: &'a str, outgoing: bool, value: bool| -> Element<'a, Message> {
                let device = id.clone();
                row![
                    row![text(from).size(14), icon(ic::TO).size(14), text(to).size(14)]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .width(Length::Fill),
                    toggler(value).on_toggle(move |v| Message::Set(Setting::Rule { device: device.clone(), outgoing, channel, on: v })),
                ]
                .align_y(Alignment::Center)
                .into()
            };
            let mut items = vec![
                direction(me, name, true, p.send.get(channel)),
                direction(name, me, false, p.receive.get(channel)),
            ];
            items.extend(options(id, p));
            body = body.push(settings_list(items));
        } else {
            body = body.push(muted(format!("Not shared with {name}, in either direction.")).size(13));
        }
        container(body)
            .padding(20)
            .width(Length::Fill)
            .style(|theme: &Theme| {
                let p = theme.extended_palette();
                container::Style {
                    background: Some(Background::Color(p.background.weaker.color)),
                    border: Border { radius: 12.0.into(), width: 1.0, color: p.background.weak.color },
                    ..container::Style::default()
                }
            })
            .into()
    }))
    .spacing(18)
    .into()
}

fn peer_set(device: &str, change: PeerChange) -> Setting {
    Setting::Peer { device: device.to_owned(), change }
}

// ── Overview ─────────────────────────────────────────────────────────────

fn overview(app: &App) -> Element<'_, Message> {
    let (status, detail): (String, String) = match &app.mode {
        Some(Mode::Controlling(id)) => (format!("Controlling {}", app.device_name(id)), "Your mouse and keyboard are on that device.".into()),
        Some(Mode::Controlled(id)) => (format!("{} is controlling this device", app.device_name(id)), "Its mouse and keyboard are on this screen.".into()),
        _ if app.config().peers.is_empty() => ("Not paired yet".into(), "Pair with your other devices on the Pairing page.".into()),
        _ if app.peers.is_empty() => ("Not connected".into(), "Connect to use your paired devices.".into()),
        _ => (
            format!("Connected to {} of {} devices", app.peers.len(), app.config().peers.len()),
            "Push the pointer past an edge with a green line on the Displays page to move to that device.".into(),
        ),
    };

    let devices: Element<'_, Message> = if app.config().peers.is_empty() {
        muted("No paired devices yet.").into()
    } else {
        column(app.config().peers.iter().map(|(id, p)| {
            let os = app.peer_os(id).map(os_name).unwrap_or("Unknown");
            let address = app.peers.get(id).map_or(p.address.as_str(), |c| c.address.as_str());
            row![
                icon(ic::LAPTOP),
                column![text(app.device_name(id)).size(14).font(MEDIUM), muted(format!("{os} · {address}")).size(12)]
                    .spacing(2)
                    .width(Length::Fill),
                device_chip(app, id),
                device_button(app, id),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        }))
        .spacing(12)
        .into()
    };

    let connection = card(
        ic::PLUG,
        "Connection",
        column![
            row![column![text(status).size(20).font(MEDIUM), muted(detail)].spacing(4).width(Length::Fill), connect_button(app).width(170)]
                .spacing(16)
                .align_y(Alignment::Center),
            devices,
            setting(
                "Share this mouse and keyboard",
                "Turn off to keep the pointer on this device for a while. Also in the tray menu.",
                toggler(!app.paused).on_toggle(|on| Message::Paused(!on)),
            ),
        ]
        .spacing(16),
    );

    let arrangement = card(
        ic::DISPLAYS,
        "Arrangement",
        column![
            arrangement_canvas(app, false, 180.0),
            row![space::horizontal(), icon_button(ic::SLIDERS, "Arrange displays", Message::Navigate(Page::Displays))],
        ]
        .spacing(10),
    );

    let mut page = column![heading(ic::OVERVIEW, "Overview", "One mouse, keyboard and clipboard across your computers.", None)].spacing(18);
    if let Update::Available(release) | Update::Installing(release) = &app.update {
        page = page.push(update_banner(app, release));
    }
    page.push(connection).push(arrangement).push(permissions_card(app)).into()
}

/// What "update" does here: install in place, or open the release page.
fn update_button<'a>(app: &App, release: &update::Release) -> Element<'a, Message> {
    if matches!(app.update, Update::Installing(_)) {
        return button(text("Installing…")).style(button::primary).into();
    }
    if update::can_install(release) {
        button(with_icon(ic::ROCKET, "Install and restart")).style(button::primary).on_press(Message::InstallUpdate).into()
    } else {
        button(with_icon(ic::ROCKET, "Get it")).style(button::primary).on_press(Message::OpenUrl(release.page.clone())).into()
    }
}

fn update_banner<'a>(app: &App, release: &update::Release) -> Element<'a, Message> {
    banner(
        row![
            column![
                text(format!("dextr {} is available", release.version)).size(14).font(MEDIUM),
                text("Update your other devices too: different versions may not connect.").size(12),
            ]
            .spacing(2)
            .width(Length::Fill),
            update_button(app, release),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        Tone::Active,
    )
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
        line("Control other devices", "Lets this mouse and keyboard move to their screens.", &app.capture),
        line("Be controlled", "Lets their mouse and keyboard work here.", &app.inject),
    ]
    .spacing(14);
    if any_denied {
        body = body.push(row![space::horizontal(), icon_button(ic::REFRESH, "Ask again", Message::RetryPermissions)]);
    }
    card(ic::SHIELD, "Permissions", body)
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

/// Two-button switch between the layout modes.
fn layout_switch(current: DisplayLayout) -> Element<'static, Message> {
    let option = |layout: DisplayLayout| {
        button(text(layout.to_string()).size(13).font(MEDIUM))
            .padding([6, 16])
            .style(if layout == current { button::primary } else { button::secondary })
            .on_press(Message::Set(Setting::Layout(layout)))
    };
    row![option(DisplayLayout::Exact), option(DisplayLayout::Proportional)].spacing(4).into()
}

fn displays(app: &App) -> Element<'_, Message> {
    let c = app.config();
    let proportional = c.layout == DisplayLayout::Proportional;
    let mut details = column![
        column![text(this_name(app)).size(14).font(MEDIUM), screen_list(&app.local_screens, "Screens appear once input permissions are allowed.")]
            .spacing(8)
    ]
    .spacing(18);
    for (id, p) in &c.peers {
        let screens = app.peers.get(id).map_or(p.screens.as_slice(), |c| c.screens.as_slice());
        details = details.push(
            column![text(app.device_name(id)).size(14).font(MEDIUM), screen_list(screens, "Not known yet: connect once.")].spacing(8),
        );
    }

    let switching = card_help(
        ic::ZAP,
        "Switching",
        "Dead corners ignore crossings near the ends of an edge, so corners stay usable. Switch delay makes you push against the edge that long before crossing; moving away cancels.",
        settings_list(vec![
            slide(
                "Dead corners",
                "",
                0.0..=120.0,
                4.0,
                c.switching.dead_corners as f32,
                format!("{} px", c.switching.dead_corners),
                |v| Setting::DeadCorners(v as u32),
            ),
            slide(
                "Switch delay",
                "",
                0.0..=1000.0,
                50.0,
                c.switching.delay_ms as f32,
                if c.switching.delay_ms == 0 { "Off".into() } else { format!("{} ms", c.switching.delay_ms) },
                |v| Setting::SwitchDelay(v as u32),
            ),
        ]),
    );

    let layout_help = if proportional {
        "Shared by all devices: a change on one updates the rest.\n\nProportional: screens sit in one row at the same height. Leaving 30% of the way down an edge enters 30% of the way down the next, whatever the sizes. Drag devices left or right to reorder."
    } else {
        "Shared by all devices: a change on one updates the rest.\n\nExact: real sizes, like the system display settings. Drag devices to any side; the pointer crosses only where edges touch (the green lines). Screens snap to aligned edges when close."
    };
    let arrangement = card_help(
        ic::DISPLAYS,
        "Arrangement",
        layout_help,
        column![
            setting("Layout", "", layout_switch(c.layout)),
            arrangement_canvas(app, true, 320.0),
        ]
        .spacing(14),
    );

    column![
        heading(ic::DISPLAYS, "Displays", "Where your devices sit, and how the pointer crosses.", None),
        arrangement,
        switching,
        card(ic::INFO, "Screens", details),
    ]
    .spacing(18)
    .into()
}

// ── Pairing ──────────────────────────────────────────────────────────────

fn pairing(app: &App) -> Element<'_, Message> {
    let mut page = column![heading(
        ic::PAIRING,
        "Pairing",
        "Pair once. After that, your devices find and connect to each other by themselves.",
        None
    )]
    .spacing(18);
    if let Some(state) = &app.pairing {
        page = page.push(pairing_prompt(app, state));
    }
    if let Some(error) = &app.pairing_error {
        page = page.push(banner(text(error.as_str()).size(13), Tone::Bad));
    }
    page.push(this_device(app))
        .push(paired_devices(app))
        .push(nearby_devices(app))
        .push(pair_by_address(app))
        .push(pairing_help())
        .into()
}

/// The code to compare, or progress while the other device is reached.
fn pairing_prompt<'a>(app: &'a App, state: &'a PairingState) -> Element<'a, Message> {
    let cancel = button("Cancel").style(button::secondary).on_press(Message::PairDecision(false));
    let body: Element<'a, Message> = match state {
        PairingState::Contacting { address } => row![text(format!("Contacting {address}…")).size(14).width(Length::Fill), cancel]
            .spacing(12)
            .align_y(Alignment::Center)
            .into(),
        PairingState::Confirm { name, os, code, incoming, we_accepted, they_accepted } => {
            let title = if *incoming {
                format!("{name} ({}) wants to pair with this device", os_name(*os))
            } else {
                format!("Pairing with {name} ({})", os_name(*os))
            };
            let status = match (we_accepted, they_accepted) {
                (true, false) => format!("Waiting for {name} to confirm…"),
                (false, true) => format!("{name} confirmed. Confirm here too if the codes match."),
                _ => format!("Check that {name} shows the same code, then confirm on both devices."),
            };
            let mut buttons = row![space::horizontal(), cancel].spacing(8);
            if !we_accepted {
                buttons = buttons.push(button(with_icon(ic::CHECK, "Codes match")).style(button::primary).on_press(Message::PairDecision(true)));
            }
            let replacing = app
                .saved
                .as_ref()
                .filter(|c| !c.peer_fingerprint.is_empty())
                .and_then(|c| c.last_peer_name.as_deref())
                .filter(|old| old != name)
                .map(|old| muted(format!("This replaces the pairing with {old}.")).size(12));
            column![text(title).size(16).font(MEDIUM), text(code.as_str()).size(44).font(Font::MONOSPACE), text(status).size(13)]
                .push(replacing)
                .push(buttons)
                .spacing(10)
                .into()
        }
    };
    banner(body, Tone::Active).padding(18).into()
}

/// An address as typed on the other device: the port only when it isn't
/// the default.
fn reachable(ip: std::net::IpAddr, port: u16) -> String {
    match (ip, port == DEFAULT_PORT) {
        (ip, true) => ip.to_string(),
        (std::net::IpAddr::V6(v6), false) => format!("[{v6}]:{port}"),
        (ip, false) => format!("{ip}:{port}"),
    }
}

fn this_device(app: &App) -> Element<'_, Message> {
    let port = app.listening.unwrap_or(DEFAULT_PORT);
    let addresses: Element<'_, Message> = if app.addresses.is_empty() {
        muted("No network connection.").into()
    } else {
        column(app.addresses.iter().map(|a| {
            let address = reachable(a.ip, port);
            row![
                text(address.clone()).font(Font::MONOSPACE).size(15).width(Length::Fill),
                muted(a.interface.as_str()).size(12),
                icon_button(ic::COPY, "Copy", Message::Copy(address)),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        }))
        .spacing(8)
        .into()
    };
    card_help(
        ic::LAPTOP,
        "This device",
        "Other devices can pair with this one at these addresses.",
        column![
            row![text(this_name(app)).size(15).font(MEDIUM), muted(os_name(Os::current())).size(12)].spacing(8).align_y(Alignment::Center),
            addresses,
            row![
                icon(ic::FINGERPRINT).size(13),
                muted(if app.fingerprint.is_empty() { "…" } else { &app.fingerprint }).size(11).font(Font::MONOSPACE),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(10),
    )
}

fn paired_devices(app: &App) -> Element<'_, Message> {
    let peers = &app.config().peers;
    if peers.is_empty() {
        return card(ic::PAIRING, "Paired devices", muted("None yet. Pair with a device below."));
    }
    card_help(
        ic::PAIRING,
        "Paired devices",
        "Paired devices connect to each other by themselves. Pair with as many as you like; arrange them on the Displays page.",
        column(peers.iter().map(|(id, p)| {
            let address = app.peers.get(id).map_or(p.address.as_str(), |c| c.address.as_str());
            column![
                row![
                    icon(ic::LAPTOP),
                    column![text(app.device_name(id)).size(15).font(MEDIUM), muted(address).size(12).font(Font::MONOSPACE)]
                        .spacing(2)
                        .width(Length::Fill),
                    device_chip(app, id),
                    device_button(app, id),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
                danger_row(app, "Forget this device", "", "Forget", Danger::ForgetPeer(id.clone())),
            ]
            .spacing(10)
            .into()
        }))
        .spacing(20),
    )
}

fn nearby_devices(app: &App) -> Element<'_, Message> {
    let can_pair = app.pairing.is_none();
    let list: Element<'_, Message> = if app.nearby.is_empty() {
        muted("Looking for other devices running dextr on this network…").into()
    } else {
        column(app.nearby.iter().map(|n| {
            let action: Element<'_, Message> = if n.paired {
                chip("Paired", Tone::Good)
            } else {
                button(with_icon(ic::PAIRING, "Pair"))
                    .style(button::primary)
                    .on_press_maybe(can_pair.then(|| Message::PairWith(n.address.to_string())))
                    .into()
            };
            row![
                icon(ic::LAPTOP),
                column![
                    text(n.name.as_str()).size(15).font(MEDIUM),
                    muted(format!("{} · {}", os_name(n.os), reachable(n.address.ip(), n.address.port()))).size(12),
                ]
                .spacing(2)
                .width(Length::Fill),
                action,
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        }))
        .spacing(12)
        .into()
    };
    card(ic::RADAR, "Nearby devices", list)
}

fn pair_by_address(app: &App) -> Element<'_, Message> {
    let can_pair = app.pairing.is_none() && !app.pair_address.trim().is_empty();
    let go = can_pair.then(|| Message::PairWith(app.pair_address.clone()));
    card_help(
        ic::NETWORK,
        "Pair by address",
        "For a device that isn't listed, for example over Tailscale or another subnet. Type an address shown on its Pairing page.",
        row![
            text_input("192.168.1.20, name.local or 100.64.0.2", &app.pair_address)
                .on_input(Message::PairAddress)
                .on_submit_maybe(go.clone())
                .font(Font::MONOSPACE)
                .padding(9),
            button(with_icon(ic::PAIRING, "Pair")).style(button::primary).on_press_maybe(go),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
}

fn pairing_help<'a>() -> Element<'a, Message> {
    const FIREWALLD: &str = "sudo firewall-cmd --permanent --add-port=47800-47801/udp && sudo firewall-cmd --reload";
    card(
        ic::INFO,
        "Not finding each other?",
        column![
            muted("Open dextr on both devices on the same network. It uses UDP ports 47800 and 47801; on macOS, allow incoming connections if asked. On Linux with a firewall, such as Fedora, allow those ports:"),
            row![
                container(text(FIREWALLD).font(Font::MONOSPACE).size(12)).padding(9).width(Length::Fill).style(container::bordered_box),
                icon_button(ic::COPY, "Copy", Message::Copy(FIREWALLD.into())),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(10),
    )
}

// ── Mouse ────────────────────────────────────────────────────────────────

fn off_banner<'a>(what: &'a str) -> Element<'a, Message> {
    banner(muted(what), Tone::Warn).into()
}

fn mouse(app: &App) -> Element<'_, Message> {
    let m = &app.config().mouse;
    let mut page = column![heading(
        ic::MOUSE,
        "Mouse",
        "How this mouse behaves on each device.",
        Some(master("Share mouse", m.enabled, Setting::MouseEnabled)),
    )]
    .spacing(18);
    if !m.enabled {
        page = page.push(off_banner("The mouse is off: the pointer stays on this device (and so does the keyboard)."));
    }
    page.push(device_cards(app, Channel::Mouse, |id, p| {
        let m = &p.mouse;
        vec![
            slide("Pointer speed", "", 0.25..=3.0, 0.05, m.speed, format!("{:.2}×", m.speed), move |v| {
                peer_set(id, PeerChange::PointerSpeed(v))
            }),
            slide("Scroll speed", "", 0.25..=3.0, 0.05, m.scroll_speed, format!("{:.2}×", m.scroll_speed), move |v| {
                peer_set(id, PeerChange::ScrollSpeed(v))
            }),
            switch("Natural scrolling", "", m.natural_scroll, move |v| peer_set(id, PeerChange::NaturalScroll(v))),
            switch("Swap left and right buttons", "", m.swap_buttons, move |v| peer_set(id, PeerChange::SwapButtons(v))),
        ]
    }))
    .into()
}

// ── Keyboard ─────────────────────────────────────────────────────────────

fn keyboard(app: &App) -> Element<'_, Message> {
    let k = &app.config().keyboard;

    let mut page = column![heading(
        ic::KEYBOARD,
        "Keyboard",
        "How this keyboard behaves on each device.",
        Some(master("Share keyboard", k.enabled, Setting::KeyboardEnabled)),
    )]
    .spacing(18);
    if !k.enabled {
        page = page.push(off_banner("The keyboard is off: keys aren't sent while the pointer is on another device. The shortcut back still works."));
    }

    let devices = device_cards(app, Channel::Keyboard, |id, p| {
        let mut items = vec![choose(
            "Modifier keys",
            "Automatic swaps Ctrl and Cmd when exactly one of the two devices is a Mac, so copy and paste stay under the same finger.",
            ModifierMap::ALL,
            p.keyboard.modifiers,
            move |v| peer_set(id, PeerChange::Modifiers(v)),
        )];
        if p.keyboard.modifiers == ModifierMap::Automatic
            && let Some(os) = app.peer_os(id)
        {
            let effective = p.keyboard.effective(Os::current(), os);
            items.push(muted(format!("With {}: {}.", os_name(os), effective.to_string().to_lowercase())).size(12).into());
        }
        items
    });

    let shortcut_keys: Element<'_, Message> = match k.return_shortcut {
        ReturnShortcut::CtrlAltEsc => row![keycap("Ctrl"), text("+").size(13), keycap("Alt"), text("+").size(13), keycap("Esc")]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        ReturnShortcut::ScrollLock => keycap("Scroll Lock"),
        ReturnShortcut::Pause => keycap("Pause"),
    };
    let shortcuts = card(
        ic::ROTATE,
        "Shortcuts",
        column![
            choose(
                "Bring the pointer back",
                "Works from any device at any time. This key isn't sent.",
                ReturnShortcut::ALL,
                k.return_shortcut,
                Setting::ReturnShortcut,
            ),
            row![space::horizontal(), shortcut_keys],
        ]
        .spacing(10),
    );

    page.push(devices)
    .push(shortcuts)
    .push(card_help(
        ic::KEYBOARD,
        "Layout",
        "Keys are sent by their position, like a USB keyboard; the other device types with its own layout. Every key and button you're holding is released whenever the pointer switches, so nothing gets stuck.",
        muted("Keys follow the other device's keyboard layout."),
    ))
    .into()
}

// ── Clipboard ────────────────────────────────────────────────────────────

fn clipboard(app: &App) -> Element<'_, Message> {
    let c = &app.config().clipboard;
    let mut page = column![heading(
        ic::CLIPBOARD,
        "Clipboard",
        "Copy on one device, paste on another.",
        Some(master("Share clipboard", c.enabled, Setting::ClipboardEnabled)),
    )]
    .spacing(18);
    if !c.enabled {
        page = page.push(off_banner("The clipboard is off: copies stay on the device they were made on."));
    }
    let devices = device_cards(app, Channel::Clipboard, |id, p| {
        let c = &p.clipboard;
        vec![
            switch("Include images", "", c.images, move |v| peer_set(id, PeerChange::ClipboardImages(v))),
            slide(
                "Largest copy",
                "Bigger copies aren't shared with this device, in either direction.",
                1.0..=256.0,
                1.0,
                c.max_mb as f32,
                format!("{} MB", c.max_mb),
                move |v| peer_set(id, PeerChange::ClipboardMaxMb(v as u32)),
            ),
        ]
    });
    page.push(devices)
    .push(card(
        ic::INFO,
        "How it works",
        muted("Whatever you copy is sent right away over the encrypted connection, so it's ready to paste wherever the pointer is. Text and images are supported; files aren't yet.").size(13),
    ))
    .into()
}

// ── Activity ─────────────────────────────────────────────────────────────

fn activity(app: &App) -> Element<'_, Message> {
    let list: Element<'_, Message> = if app.log.is_empty() {
        muted("Nothing yet.").into()
    } else {
        column(app.log.iter().map(|l| text(l).size(13).into())).spacing(8).into()
    };
    let clear = button(with_icon(ic::TRASH, "Clear")).style(button::secondary).on_press_maybe((!app.log.is_empty()).then_some(Message::ClearLog));
    column![
        heading(ic::ACTIVITY, "Activity", "Connections, permissions and errors, newest first.", Some(clear.into())),
        card(ic::ACTIVITY, "Log", list),
    ]
    .spacing(18)
    .into()
}

// ── Settings ─────────────────────────────────────────────────────────────

fn settings(app: &App) -> Element<'_, Message> {
    let c = app.config();
    let port_changed = app.saved.as_ref().is_some_and(|c| c.port.to_string() != app.form.port.trim());

    let device = card(
        ic::LAPTOP,
        "Device",
        column![
            field("Name", text_input("Shown on your other devices", &app.form.name).on_input(Message::Name).on_submit(Message::Save).padding(9)),
            field("Port (UDP)", text_input("47800", &app.form.port).on_input(Message::Port).on_submit(Message::Save).padding(9).width(140)),
            muted(match (app.listening, port_changed) {
                (_, true) => "The new port takes effect after restarting dextr.".to_string(),
                (Some(port), false) => format!("Listening on UDP port {port}."),
                (None, false) => "Not listening. Another app may be using this port.".to_string(),
            }),
            row![space::horizontal(), button(with_icon(ic::CHECK, "Save")).style(button::primary).on_press_maybe(app.unsaved().then_some(Message::Save))],
        ]
        .spacing(14),
    );

    let startup_desc = if autostart::supported() {
        ""
    } else {
        "Not available on this OS yet."
    };
    let startup = card(
        ic::ROCKET,
        "Startup and connection",
        settings_list(vec![
            setting(
                "Launch at login",
                startup_desc,
                toggler(app.launch_at_login).on_toggle_maybe(autostart::supported().then_some(Message::LaunchAtLogin)),
            ),
            switch("Connect automatically", "", c.connection.auto_connect, Setting::AutoConnect),
            switch(
                "Reconnect automatically",
                "",
                c.connection.auto_reconnect,
                Setting::AutoReconnect,
            ),
        ]),
    );

    let privacy = card_help(
        ic::SHIELD,
        "Privacy and security",
        "Connections are encrypted with QUIC (TLS 1.3). Only the device whose fingerprint you paired can connect.",
        settings_list(vec![
            switch(
                "Allow being controlled",
                "Let paired devices' mouse and keyboard work on this device at all. Per-device choices are on the Mouse and Keyboard pages.",
                c.connection.allow_control,
                Setting::AllowControl,
            ),
            switch(
                "Accept incoming connections",
                "",
                c.connection.allow_incoming,
                Setting::AllowIncoming,
            ),
        ]),
    );

    let appearance = card(
        ic::PALETTE,
        "Appearance",
        settings_list(vec![choose("Theme", "", Appearance::ALL, c.appearance, Setting::Appearance)]),
    );

    let config_dir = dextr_core::config::dir().map(|d| d.display().to_string()).unwrap_or_else(|e| e.to_string());
    let files = card_help(
        ic::FOLDER,
        "Files",
        "Settings and this device's certificate are stored here. Keep key.der private.",
        row![
            text(config_dir).font(Font::MONOSPACE).size(13).width(Length::Fill),
            icon_button(ic::FOLDER, "Open folder", Message::OpenConfigFolder),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    );

    let reset = card(
        ic::ROTATE,
        "Reset",
        settings_list(vec![
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
            danger_row(app, "Reset all settings", "Restore defaults. Keeps pairings, name and port.", "Reset", Danger::ResetSettings),
        ]),
    );

    let status: String = match &app.update {
        Update::Unknown => "Not checked yet.".into(),
        Update::Checking => "Checking…".into(),
        Update::UpToDate => "Up to date.".into(),
        Update::Available(r) => format!("dextr {} is available.", r.version),
        Update::Installing(r) => format!("Installing dextr {}…", r.version),
        Update::Failed(e) => format!("Couldn't check: {e}"),
    };
    let action: Element<'_, Message> = match &app.update {
        Update::Available(r) | Update::Installing(r) => update_button(app, r),
        Update::Checking => button(with_icon(ic::REFRESH, "Check now")).style(button::secondary).into(),
        _ => icon_button(ic::REFRESH, "Check now", Message::CheckUpdates { manual: true }).into(),
    };
    let about = card(
        ic::INFO,
        "About and updates",
        column![
            row![
                column![text(format!("dextr {}", update::current())).size(14).font(MEDIUM), muted(status).size(12)]
                    .spacing(2)
                    .width(Length::Fill),
                action,
            ]
            .spacing(12)
            .align_y(Alignment::Center),
            switch("Check for updates automatically", "", c.check_updates, Setting::CheckUpdates),
        ]
        .spacing(14),
    );

    column![
        heading(ic::SETTINGS, "Settings", "General options for this device.", None),
        device,
        startup,
        privacy,
        appearance,
        files,
        reset,
        about,
    ]
    .spacing(18)
    .into()
}
