//! Display arrangement: this device's screens and the peer's, drawn to scale
//! in pixels like the OS display settings. Drag the peer's screens anywhere
//! around yours; on release they snap to the nearest side, stay where they
//! were dropped along it, and align to the edges or center when close.

use dextr_core::edge::Link;
use dextr_proto::{Rect, Side};
use iced::{
    Color, Point, Rectangle, Renderer, Size, Theme, Vector,
    alignment, mouse,
    widget::canvas::{self, Action, Event, Frame, Geometry, LineDash, Path, Stroke, Text},
};

/// Shown when this device's screens aren't known yet.
const PLACEHOLDER: Rect = Rect::new(0, 0, 1920, 1080);

/// Snap to aligned edges or center within this fraction of the edge length.
const SNAP: f32 = 0.04;

pub struct Arrangement<'a, M> {
    pub local: &'a [Rect],
    /// The peer's screens, only while connected.
    pub peer: Option<&'a [Rect]>,
    pub link: Link,
    pub local_label: &'a str,
    pub peer_label: &'a str,
    pub on_change: Option<fn(Link) -> M>,
}

#[derive(Default)]
pub struct State {
    /// Cursor offset from the peer group's top-left while dragging.
    grab: Option<Vector>,
    /// Peer group's top-left while dragging, in canvas pixels.
    drag_at: Option<Point>,
}

/// Screens in canvas pixels, plus the mapping from our pixel coordinates.
struct Layout {
    local: Vec<Rectangle>,
    peer: Vec<Rectangle>,
    local_bounds: Rectangle,
    peer_bounds: Option<Rectangle>,
    ob: Rect,
    tb: Option<Rect>,
    origin: Point,
    scale: f32,
}

impl Layout {
    fn to_canvas(&self, x: f32, y: f32, w: f32, h: f32) -> Rectangle {
        Rectangle::new(
            Point::new(self.origin.x + x * self.scale, self.origin.y + y * self.scale),
            Size::new(w * self.scale, h * self.scale),
        )
    }

    fn to_world(&self, p: Point) -> (f32, f32) {
        ((p.x - self.origin.x) / self.scale, (p.y - self.origin.y) / self.scale)
    }

    /// The peer's bounds placed by `link`, in canvas pixels.
    fn peer_rect(&self, link: Link) -> Option<Rectangle> {
        let tb = self.tb?;
        let (x, y) = link.place(self.ob, tb);
        Some(self.to_canvas(x as f32, y as f32, tb.w as f32, tb.h as f32))
    }
}

/// Proportional: the screens of one device side by side in a row, in their
/// left-to-right order, each scaled to height 1. Returns (x, width) per
/// screen in the original order, and the row's width.
fn row(screens: &[Rect]) -> (Vec<(f32, f32)>, f32) {
    const GAP: f32 = 0.05;
    let mut order: Vec<usize> = (0..screens.len()).collect();
    order.sort_by_key(|&i| (screens[i].x, screens[i].y));
    let mut out = vec![(0.0, 0.0); screens.len()];
    let mut x = 0.0;
    for (n, &i) in order.iter().enumerate() {
        if n > 0 {
            x += GAP;
        }
        let w = screens[i].w as f32 / screens[i].h.max(1) as f32;
        out[i] = (x, w);
        x += w;
    }
    (out, x)
}

impl<M> Arrangement<'_, M> {
    fn layout(&self, size: Size) -> Layout {
        self.layout_for(self.link, size)
    }

    fn layout_for(&self, link: Link, size: Size) -> Layout {
        if link.proportional {
            return self.row_layout(link.side, size);
        }
        let local = if self.local.is_empty() { vec![PLACEHOLDER] } else { self.local.to_vec() };
        let ob = Rect::bounds(&local).unwrap();
        let peer = self.peer.filter(|p| !p.is_empty());
        let tb = peer.and_then(Rect::bounds);

        // Everything in our pixel coordinates.
        let (mut x0, mut y0) = (ob.x as f32, ob.y as f32);
        let (mut x1, mut y1) = (ob.right(), ob.bottom());
        let mut place = (0, 0);
        if let Some(tb) = tb {
            place = link.place(ob, tb);
            x0 = x0.min(place.0 as f32);
            y0 = y0.min(place.1 as f32);
            x1 = x1.max((place.0 + tb.w as i32) as f32);
            y1 = y1.max((place.1 + tb.h as i32) as f32);
        }
        // Leave room around a lone screen so it doesn't fill the canvas.
        let fill = if tb.is_some() { (0.86, 0.80) } else { (0.55, 0.62) };
        let scale = ((size.width * fill.0) / (x1 - x0)).min((size.height * fill.1) / (y1 - y0));
        let origin = Point::new(
            size.width / 2.0 - (x0 + x1) / 2.0 * scale,
            size.height / 2.0 - (y0 + y1) / 2.0 * scale,
        );

        let mut layout = Layout {
            local: Vec::new(),
            peer: Vec::new(),
            local_bounds: Rectangle::default(),
            peer_bounds: None,
            ob,
            tb,
            origin,
            scale,
        };
        layout.local = local.iter().map(|r| layout.to_canvas(r.x as f32, r.y as f32, r.w as f32, r.h as f32)).collect();
        layout.local_bounds = layout.to_canvas(ob.x as f32, ob.y as f32, ob.w as f32, ob.h as f32);
        if let (Some(peer), Some(tb)) = (peer, tb) {
            let (dx, dy) = ((place.0 - tb.x) as f32, (place.1 - tb.y) as f32);
            layout.peer = peer.iter().map(|r| layout.to_canvas(r.x as f32 + dx, r.y as f32 + dy, r.w as f32, r.h as f32)).collect();
            layout.peer_bounds = layout.peer_rect(link);
        }
        layout
    }

    /// Proportional: both devices' screens in one flat row, all the same
    /// height, the other device on the left or right.
    fn row_layout(&self, side: Side, size: Size) -> Layout {
        let local = if self.local.is_empty() { vec![PLACEHOLDER] } else { self.local.to_vec() };
        let peer = self.peer.filter(|p| !p.is_empty()).map(<[Rect]>::to_vec);
        let (local_row, local_w) = row(&local);
        let peer_row = peer.as_deref().map(row);
        const BETWEEN: f32 = 0.18;

        let total = local_w + peer_row.as_ref().map_or(0.0, |(_, w)| BETWEEN + w);
        let fill = if peer.is_some() { 0.88 } else { 0.5 };
        let scale = (size.width * fill / total).min(size.height * 0.62);
        let left = (size.width - total * scale) / 2.0;
        let top = (size.height - scale) / 2.0 + 8.0;
        let (local_x, peer_x) = match side {
            Side::Left => (left + peer_row.as_ref().map_or(0.0, |(_, w)| (w + BETWEEN) * scale), left),
            _ => (left, left + (local_w + BETWEEN) * scale),
        };
        let place = |x0: f32, (x, w): (f32, f32)| Rectangle::new(Point::new(x0 + x * scale, top), Size::new(w * scale, scale));

        let mut layout = Layout {
            local: local_row.iter().map(|&r| place(local_x, r)).collect(),
            peer: Vec::new(),
            local_bounds: Rectangle::new(Point::new(local_x, top), Size::new(local_w * scale, scale)),
            peer_bounds: None,
            ob: Rect::bounds(&local).unwrap(),
            tb: peer.as_deref().and_then(Rect::bounds),
            origin: Point::ORIGIN,
            scale: 1.0,
        };
        if let Some((peer_row, peer_w)) = peer_row {
            layout.peer = peer_row.iter().map(|&r| place(peer_x, r)).collect();
            layout.peer_bounds = Some(Rectangle::new(Point::new(peer_x, top), Size::new(peer_w * scale, scale)));
        }
        layout
    }

    /// Placement for the peer group dropped with its top-left at `top_left`.
    fn link_at(&self, layout: &Layout, top_left: Point) -> Option<Link> {
        if self.link.proportional {
            // Left or right of this device's row, whichever is nearer.
            let peer = layout.peer_bounds?;
            let center = top_left.x + peer.width / 2.0;
            let side = if center < layout.local_bounds.center_x() { Side::Left } else { Side::Right };
            return Some(Link { side, offset: None, proportional: true });
        }
        let (ob, tb) = (layout.ob, layout.tb?);
        let (x, y) = layout.to_world(top_left);

        // Nearest side, from the centers.
        let dx = (x + tb.w as f32 / 2.0 - (ob.x as f32 + ob.w as f32 / 2.0)) / ((ob.w + tb.w) as f32 / 2.0);
        let dy = (y + tb.h as f32 / 2.0 - (ob.y as f32 + ob.h as f32 / 2.0)) / ((ob.h + tb.h) as f32 / 2.0);
        let side = if dx.abs() >= dy.abs() {
            if dx >= 0.0 { Side::Right } else { Side::Left }
        } else if dy >= 0.0 {
            Side::Bottom
        } else {
            Side::Top
        };

        // Where along that side, snapping to centered or aligned ends.
        let (start, ol, tl) = if side.is_horizontal() {
            (y - ob.y as f32, ob.h as i32, tb.h as i32)
        } else {
            (x - ob.x as f32, ob.w as i32, tb.w as i32)
        };
        let snap = ol.max(tl) as f32 * SNAP;
        let candidates = [((ol - tl) / 2, None), (0, Some(0)), (ol - tl, Some(ol - tl))];
        if let Some(&(_, offset)) = candidates.iter().find(|(at, _)| (start - *at as f32).abs() <= snap) {
            return Some(Link { side, offset, proportional: false });
        }
        let free = Link { side, offset: Some(start.round() as i32), proportional: false };
        Some(Link { side, offset: Some(free.offset_for(ob, tb)), proportional: false })
    }
}

fn screen(frame: &mut Frame, r: Rectangle, fill: Color, border: Color, label: &str, text: Color) {
    let path = Path::rounded_rectangle(r.position(), r.size(), 6.0.into());
    frame.fill(&path, fill);
    frame.stroke(&path, Stroke::default().with_color(border).with_width(2.0));
    if r.width > 50.0 && r.height > 24.0 {
        frame.fill_text(Text {
            content: label.into(),
            position: r.center(),
            color: text,
            size: 12.0.into(),
            align_x: alignment::Horizontal::Center.into(),
            align_y: alignment::Vertical::Center,
            ..Text::default()
        });
    }
}

fn caption(frame: &mut Frame, over: Rectangle, label: &str, color: Color) {
    frame.fill_text(Text {
        content: label.into(),
        position: Point::new(over.center_x(), over.y - 8.0),
        color,
        size: 14.0.into(),
        align_x: alignment::Horizontal::Center.into(),
        align_y: alignment::Vertical::Bottom,
        ..Text::default()
    });
}

/// The stretch of our edge that touches the peer, in canvas pixels.
fn shared_edge(side: Side, l: Rectangle, p: Rectangle) -> (Point, Point) {
    match side {
        Side::Right | Side::Left => {
            let x = if side == Side::Right { l.x + l.width } else { l.x };
            (Point::new(x, l.y.max(p.y)), Point::new(x, (l.y + l.height).min(p.y + p.height)))
        }
        Side::Bottom | Side::Top => {
            let y = if side == Side::Bottom { l.y + l.height } else { l.y };
            (Point::new(l.x.max(p.x), y), Point::new((l.x + l.width).min(p.x + p.width), y))
        }
    }
}

impl<M> canvas::Program<M> for Arrangement<'_, M> {
    type State = State;

    fn update(&self, state: &mut State, event: &Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<Action<M>> {
        let on_change = self.on_change?;
        let layout = self.layout(bounds.size());
        let peer_bounds = layout.peer_bounds?;
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let at = cursor.position_in(bounds)?;
                if layout.peer.iter().any(|r| r.contains(at)) {
                    state.grab = Some(at - peer_bounds.position());
                    state.drag_at = Some(peer_bounds.position());
                    return Some(Action::request_redraw().and_capture());
                }
                None
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let grab = state.grab?;
                let at = cursor.position_in(bounds).or_else(|| {
                    cursor.position().map(|p| Point::new(p.x - bounds.x, p.y - bounds.y))
                })?;
                state.drag_at = Some(at - grab);
                Some(Action::request_redraw().and_capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.grab.take()?;
                let top_left = state.drag_at.take()?;
                let link = self.link_at(&layout, top_left)?;
                Some(Action::publish(on_change(link)).and_capture())
            }
            _ => None,
        }
    }

    fn draw(&self, state: &State, renderer: &Renderer, theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let layout = self.layout(bounds.size());
        let mut frame = Frame::new(renderer, bounds.size());
        let text = palette.background.base.text;
        let muted = palette.background.strong.color;

        for (i, r) in layout.local.iter().enumerate() {
            let label = match self.local.get(i) {
                Some(s) => format!("{} × {}", s.w, s.h),
                None => "Unknown".into(),
            };
            screen(&mut frame, *r, palette.primary.weak.color, palette.primary.strong.color, &label, palette.primary.weak.text);
        }
        caption(&mut frame, layout.local_bounds, self.local_label, text);

        let Some(peer_bounds) = layout.peer_bounds else {
            frame.fill_text(Text {
                content: "Connect to your other device to arrange its screens.".into(),
                position: Point::new(bounds.width / 2.0, layout.local_bounds.y + layout.local_bounds.height + 22.0),
                color: muted,
                size: 13.0.into(),
                align_x: alignment::Horizontal::Center.into(),
                align_y: alignment::Vertical::Top,
                ..Text::default()
            });
            return vec![frame.into_geometry()];
        };

        // While dragging, show where the screens will snap to.
        let target = state.drag_at.and_then(|at| self.link_at(&layout, at)).unwrap_or(self.link);
        let target_layout = target.proportional.then(|| self.layout_for(target, bounds.size()));
        let target_rect = match &target_layout {
            Some(l) => l.peer_bounds,
            None => layout.peer_rect(target),
        };
        if state.drag_at.is_some()
            && let Some(r) = target_rect
        {
            let path = Path::rounded_rectangle(r.position(), r.size(), 6.0.into());
            let mut stroke = Stroke::default().with_color(palette.success.strong.color).with_width(2.0);
            stroke.line_dash = LineDash { segments: &[8.0, 6.0], offset: 0 };
            frame.stroke(&path, stroke);
        }

        // The part of the edge the pointer crosses.
        let edge = match &target_layout {
            // Proportional: the whole outer edge of this device's row.
            Some(l) => {
                let outer = if target.side == Side::Left {
                    l.local.iter().copied().min_by(|a, b| a.x.total_cmp(&b.x))
                } else {
                    l.local.iter().copied().max_by(|a, b| (a.x + a.width).total_cmp(&(b.x + b.width)))
                };
                outer.map(|r| {
                    let x = if target.side == Side::Left { r.x } else { r.x + r.width };
                    (Point::new(x, r.y), Point::new(x, r.y + r.height))
                })
            }
            None => target_rect.map(|p| shared_edge(target.side, layout.local_bounds, p)),
        };
        if let Some((a, b)) = edge {
            frame.stroke(&Path::line(a, b), Stroke::default().with_color(palette.success.base.color).with_width(4.0));
        }

        let shift = match state.drag_at {
            Some(at) => at - peer_bounds.position(),
            None => Vector::ZERO,
        };
        let peer = self.peer.unwrap_or_default();
        for (i, r) in layout.peer.iter().enumerate() {
            let label = peer.get(i).map(|s| format!("{} × {}", s.w, s.h)).unwrap_or_default();
            screen(&mut frame, *r + shift, palette.success.weak.color, palette.success.strong.color, &label, palette.success.weak.text);
        }
        caption(&mut frame, peer_bounds + shift, self.peer_label, text);

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if self.on_change.is_none() {
            return mouse::Interaction::default();
        }
        if state.grab.is_some() {
            return mouse::Interaction::Grabbing;
        }
        let layout = self.layout(bounds.size());
        match cursor.position_in(bounds) {
            Some(at) if layout.peer.iter().any(|r| r.contains(at)) => mouse::Interaction::Grab,
            _ => mouse::Interaction::default(),
        }
    }
}
