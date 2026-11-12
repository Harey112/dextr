//! Window capture on Linux: the ScreenCast portal's window picker, PipeWire
//! for the pixels, and (on KDE Plasma) KWin for the title and position.

use std::{sync::Arc, time::Duration};

use anyhow::Context as _;
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use tokio::sync::{oneshot, watch};

use super::{
    kwin::{KWinWatch, KWinWindow},
    pipewire,
};
use crate::{
    spawn_backend_thread,
    window::{CapturedFrame, ShareableWindow, WindowCapture, WindowGeometry},
};

pub(crate) async fn shareable_windows() -> anyhow::Result<Option<Vec<ShareableWindow>>> {
    Ok(None)
}

pub(crate) async fn capture_window(_id: Option<u64>) -> anyhow::Result<WindowCapture> {
    let (ready_tx, ready_rx) = oneshot::channel();
    spawn_backend_thread("dextr-window", move || run(ready_tx))?;
    ready_rx.await.context("the window capture thread exited")?
}

/// Which of KWin's geometries matches the captured pixels.
#[derive(Clone, Copy)]
enum Fit {
    Frame,
    Buffer,
}

fn geometry(w: &KWinWindow, fit: Fit) -> WindowGeometry {
    let g = match fit {
        Fit::Frame => w.frame,
        Fit::Buffer => w.buffer,
    };
    WindowGeometry { x: g[0], y: g[1], width: g[2], height: g[3] }
}

/// The window whose size matches the stream's (logical) size, preferring the
/// active one.
fn find(windows: &[KWinWindow], size: (i32, i32)) -> Option<(KWinWindow, Fit)> {
    let near = |g: [f32; 4]| (g[2] - size.0 as f32).abs() <= 2.0 && (g[3] - size.1 as f32).abs() <= 2.0;
    let mut matches: Vec<(KWinWindow, Fit)> = windows
        .iter()
        .filter_map(|w| {
            if near(w.frame) {
                Some((w.clone(), Fit::Frame))
            } else if near(w.buffer) {
                Some((w.clone(), Fit::Buffer))
            } else {
                None
            }
        })
        .collect();
    matches.sort_by_key(|(w, _)| !w.active);
    matches.into_iter().next()
}

async fn run(ready: oneshot::Sender<anyhow::Result<WindowCapture>>) {
    let portal = async {
        let proxy = Screencast::new().await.context("the ScreenCast portal is unavailable")?;
        let session = proxy.create_session(Default::default()).await?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::HIDDEN)
                    .set_sources(SourceType::WINDOW)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await?
            .response()?;
        let streams = proxy.start(&session, None, Default::default()).await?.response().map_err(|e| match e {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => anyhow::anyhow!("no window was chosen"),
            e => e.into(),
        })?;
        let stream = streams.streams().first().cloned().context("no window was chosen")?;
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        anyhow::Ok((session, stream, fd))
    };
    let (session, stream, fd) = match portal.await {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let (frames_tx, frames) = watch::channel::<Option<Arc<CapturedFrame>>>(None);
    let (ended_tx, mut pw_ended) = oneshot::channel();
    let capture = match pipewire::Capture::start(fd, stream.pipe_wire_node_id(), frames_tx, ended_tx) {
        Ok(c) => c,
        Err(e) => {
            let _ = session.close().await;
            let _ = ready.send(Err(e));
            return;
        }
    };

    // Find the window in KWin to learn its title and where it is.
    let mut kwin = match KWinWatch::start().await {
        Ok(k) => Some(k),
        Err(e) => {
            tracing::info!("window position unavailable: {e:#}");
            None
        }
    };
    let mut matched: Option<(String, Fit)> = None;
    let (mut title, mut app) = (String::from("Shared window"), String::new());
    let (geometry_tx, geometry_rx) = watch::channel(None);
    if let (Some(k), Some(size)) = (kwin.as_mut(), stream.size()) {
        let mut seen = Vec::new();
        let deadline = tokio::time::sleep(Duration::from_millis(700));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                Some(w) = k.windows.recv() => seen.push(w),
                () = &mut deadline => break,
            }
        }
        match find(&seen, size) {
            Some((w, fit)) => {
                title = w.caption.clone();
                app = w.app.clone();
                geometry_tx.send_replace(Some(geometry(&w, fit)));
                matched = Some((w.id, fit));
            }
            None => tracing::info!("no KWin window matches the stream size {size:?}; input on it is off"),
        }
    }

    let (stop_tx, mut stop) = oneshot::channel();
    let (ended_tx, ended) = oneshot::channel();
    let _ = ready.send(Ok(WindowCapture { title, app, frames, geometry: geometry_rx, ended, stop: Some(stop_tx) }));

    let reason = loop {
        tokio::select! {
            _ = &mut stop => break String::from("stopped"),
            r = &mut pw_ended => break r.unwrap_or_else(|_| "the stream ended".into()),
            Some(w) = async { match kwin.as_mut() { Some(k) => k.windows.recv().await, None => std::future::pending().await } } => {
                if let Some((id, fit)) = &matched && *id == w.id {
                    geometry_tx.send_replace(Some(geometry(&w, *fit)));
                }
            }
        }
    };
    drop(capture);
    if let Some(k) = kwin {
        k.stop().await;
    }
    let _ = session.close().await;
    let _ = ended_tx.send(reason);
}
