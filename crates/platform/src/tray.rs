//! The tray icon (system tray / menu bar), so dextr can run with its window
//! closed.
//!
//! * Linux: the StatusNotifierItem protocol over D-Bus, in pure Rust. KDE
//!   Plasma, Xfce, Cinnamon, MATE, LXQt, Budgie and wlroots bars show it;
//!   GNOME does with the AppIndicator extension (built in on Ubuntu, Fedora
//!   and others).
//! * macOS (menu bar) and Windows (notification area): the native tray, via
//!   `tray-icon`. These must be created on the main thread once the app's
//!   event loop runs.

/// What the user picked in the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Show dextr's window.
    Open,
    /// Pause or resume sharing this mouse and keyboard.
    SetPaused(bool),
    Quit,
}

/// What the tray shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrayState {
    /// One line, for example "Connected to MacBook".
    pub status: String,
    pub paused: bool,
}

const ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/dextr-64.png");

fn icon_rgba() -> Option<(u32, u32, Vec<u8>)> {
    let image = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?.into_rgba8();
    let (w, h) = image.dimensions();
    Some((w, h, image.into_raw()))
}

#[cfg(target_os = "linux")]
pub use linux::{TrayHandle, start_tray};
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub use native::{TrayHandle, start_tray};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub struct TrayHandle;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
impl TrayHandle {
    pub fn update(&self, _state: TrayState) {}
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn start_tray(_actions: tokio::sync::mpsc::UnboundedSender<TrayAction>, _state: TrayState) -> anyhow::Result<TrayHandle> {
    anyhow::bail!("no tray on this OS")
}

#[cfg(target_os = "linux")]
mod linux {
    use ksni::{
        TrayMethods,
        menu::{CheckmarkItem, MenuItem, StandardItem},
    };
    use tokio::sync::{mpsc, oneshot};

    use super::{TrayAction, TrayState, icon_rgba};

    struct Tray {
        state: TrayState,
        actions: mpsc::UnboundedSender<TrayAction>,
        icon: Vec<ksni::Icon>,
    }

    impl ksni::Tray for Tray {
        fn id(&self) -> String {
            "dextr".into()
        }

        fn title(&self) -> String {
            "dextr".into()
        }

        fn icon_name(&self) -> String {
            // The installed icon theme entry; the pixmap is the fallback.
            crate::tray::APP_ICON_NAME.into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icon.clone()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "dextr".into(), description: self.state.status.clone(), ..Default::default() }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.actions.send(TrayAction::Open);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem { label: self.state.status.clone(), enabled: false, ..Default::default() }.into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Open dextr".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.actions.send(TrayAction::Open);
                    }),
                    ..Default::default()
                }
                .into(),
                CheckmarkItem {
                    label: "Pause sharing".into(),
                    checked: self.state.paused,
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.actions.send(TrayAction::SetPaused(!t.state.paused));
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit dextr".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.actions.send(TrayAction::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// Updates the tray. Dropping it removes the tray icon.
    pub struct TrayHandle {
        updates: mpsc::UnboundedSender<TrayState>,
    }

    impl TrayHandle {
        pub fn update(&self, state: TrayState) {
            let _ = self.updates.send(state);
        }
    }

    /// Starts the tray icon, or fails if the desktop has no tray (for
    /// example GNOME without the AppIndicator extension).
    pub fn start_tray(actions: mpsc::UnboundedSender<TrayAction>, state: TrayState) -> anyhow::Result<TrayHandle> {
        // ARGB32, big-endian, as the protocol wants.
        let icon = icon_rgba()
            .map(|(w, h, rgba)| {
                let data = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
                vec![ksni::Icon { width: w as i32, height: h as i32, data }]
            })
            .unwrap_or_default();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (updates, mut updates_rx) = mpsc::unbounded_channel::<TrayState>();
        crate::spawn_backend_thread("dextr-tray", move || async move {
            let handle = match (Tray { state, actions, icon }).spawn().await {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("{e}")));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            let (_closed_tx, _closed) = oneshot::channel::<()>();
            while let Some(state) = updates_rx.recv().await {
                handle.update(|t| t.state = state).await;
            }
            handle.shutdown().await;
        })?;
        ready_rx.recv()??;
        Ok(TrayHandle { updates })
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tokio::sync::mpsc;
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
        menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };

    use super::{TrayAction, TrayState, icon_rgba};

    /// Lives on the main thread, where it was created.
    pub struct TrayHandle {
        _icon: TrayIcon,
        status: MenuItem,
        pause: CheckMenuItem,
        paused: Arc<AtomicBool>,
    }

    impl TrayHandle {
        /// Call on the main thread.
        pub fn update(&self, state: TrayState) {
            self.status.set_text(&state.status);
            self.pause.set_checked(state.paused);
            self.paused.store(state.paused, Ordering::Relaxed);
            let _ = self._icon.set_tooltip(Some(format!("dextr — {}", state.status)));
        }
    }

    /// Creates the tray icon. Call on the main thread while the event loop
    /// runs.
    pub fn start_tray(actions: mpsc::UnboundedSender<TrayAction>, state: TrayState) -> anyhow::Result<TrayHandle> {
        let status = MenuItem::new(&state.status, false, None);
        let open = MenuItem::new("Open dextr", true, None);
        let pause = CheckMenuItem::new("Pause sharing", true, state.paused, None);
        let quit = MenuItem::new("Quit dextr", true, None);
        let menu = Menu::new();
        menu.append_items(&[&status, &PredefinedMenuItem::separator(), &open, &pause, &PredefinedMenuItem::separator(), &quit])?;

        let (open_id, pause_id, quit_id) = (open.id().clone(), pause.id().clone(), quit.id().clone());
        // Menu items can't leave the main thread; track the state here.
        let paused = Arc::new(AtomicBool::new(state.paused));
        let paused_now = paused.clone();
        let menu_actions = actions.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = if event.id == open_id {
                TrayAction::Open
            } else if event.id == pause_id {
                TrayAction::SetPaused(!paused_now.load(Ordering::Relaxed))
            } else if event.id == quit_id {
                TrayAction::Quit
            } else {
                return;
            };
            let _ = menu_actions.send(action);
        }));
        // Windows: a left click opens the window (the menu is on the right).
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                let _ = actions.send(TrayAction::Open);
            }
        }));

        let icon = icon_rgba().and_then(|(w, h, rgba)| Icon::from_rgba(rgba, w, h).ok());
        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(format!("dextr — {}", state.status))
            .with_menu_on_left_click(cfg!(target_os = "macos"));
        if let Some(icon) = icon {
            builder = builder.with_icon(icon);
        }
        let icon = builder.build()?;
        Ok(TrayHandle { _icon: icon, status, pause, paused })
    }
}

/// The icon theme name installed with the launcher entry (Linux).
#[allow(dead_code)]
pub(crate) const APP_ICON_NAME: &str = "dev.dextr.dextr";
