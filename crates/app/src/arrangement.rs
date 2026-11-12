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

impl<M> Arrangement<'_, M> {
    fn layout(&self, size: Size) -> Layout {
        let local = if self.local.is_empty() { vec![PLACEHOLDER] } else { self.local.to_vec() };
        let ob = Rect::bounds(&local).unwrap();
        let peer = self.peer.filter(|p| !p.is_empty());
        let tb = peer.and_then(Rect::bounds);

        // Everything in our pixel coordinates.
        let (mut x0, mut y0) = (ob.x as f32, ob.y as f32);
        let (mut x1, mut y1) = (ob.right(), ob.bottom());
        let mut place = (0, 0);
        if let Some(tb) = tb {
            place = self.link.place(ob, tb);
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
            layout.peer_bounds = layout.peer_rect(self.link);
        }
        layout
    }

    /// Placement for the peer group dropped with its top-left at `top_left`.
    fn link_at(layout: &Layout, top_left: Point) -> Option<Link> {
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
            return Some(Link { side, offset });
        }
        let free = Link { side, offset: Some(start.round() as i32) };
        Some(Link { side, offset: Some(free.offset_for(ob, tb)) })
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
                let link = Self::link_at(&layout, top_left)?;
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
        let target = state.drag_at.and_then(|at| Self::link_at(&layout, at)).unwrap_or(self.link);
        if state.drag_at.is_some()
            && let Some(r) = layout.peer_rect(target)
        {
            let path = Path::rounded_rectangle(r.position(), r.size(), 6.0.into());
            let mut stroke = Stroke::default().with_color(palette.success.strong.color).with_width(2.0);
            stroke.line_dash = LineDash { segments: &[8.0, 6.0], offset: 0 };
            frame.stroke(&path, stroke);
        }

        // The part of the edge the pointer crosses.
        if let Some(p) = layout.peer_rect(target) {
            let (a, b) = shared_edge(target.side, layout.local_bounds, p);
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
