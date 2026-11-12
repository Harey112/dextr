//! Display arrangement: every paired device's screens in one shared layout,
//! drawn to scale.
//!
//! * Exact: drag another device anywhere; on release it snaps to the nearest
//!   device's edge, stays where it was dropped along it, and aligns to the
//!   edges or center when close.
//! * Proportional: the devices sit in one row at the same height; drag one
//!   left or right to change the order.
//!
//! The green lines are where the pointer crosses between devices.

use std::collections::{BTreeMap, BTreeSet};

use dextr_core::world::{Area2, Arrangement as Shared, DeviceId, World};
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
/// How much of two edges must touch at least.
const MIN_OVERLAP: f32 = 64.0;

pub struct Arrangement<'a, M> {
    /// This device.
    pub me: &'a str,
    /// Every device's screens (in its own pixels).
    pub screens: BTreeMap<DeviceId, Vec<Rect>>,
    pub names: BTreeMap<DeviceId, String>,
    pub connected: BTreeSet<DeviceId>,
    pub shared: &'a Shared,
    pub proportional: bool,
    pub new_side: Side,
    pub on_change: Option<fn(Shared) -> M>,
}

#[derive(Default)]
pub struct State {
    /// The device being dragged and the cursor's offset from its top-left.
    grab: Option<(DeviceId, Vector)>,
    /// Its top-left while dragging, in canvas pixels.
    drag_at: Option<Point>,
}

/// The shared space mapped onto the canvas.
struct View {
    world: World,
    origin: Point,
    scale: f32,
}

impl View {
    fn to_canvas(&self, a: Area2) -> Rectangle {
        Rectangle::new(
            Point::new(self.origin.x + a.x * self.scale, self.origin.y + a.y * self.scale),
            Size::new(a.w * self.scale, a.h * self.scale),
        )
    }

    fn to_world(&self, p: Point) -> (f32, f32) {
        ((p.x - self.origin.x) / self.scale, (p.y - self.origin.y) / self.scale)
    }

    /// The device whose screens are at canvas point `p`.
    fn device_at(&self, p: Point) -> Option<&dextr_core::world::Device> {
        self.world.devices.iter().find(|d| d.areas().into_iter().any(|a| self.to_canvas(a).contains(p)))
    }
}

fn union(areas: impl Iterator<Item = Area2>) -> Option<Area2> {
    areas.reduce(|o, a| {
        let (x, y) = (o.x.min(a.x), o.y.min(a.y));
        Area2 { x, y, w: o.right().max(a.right()) - x, h: o.bottom().max(a.bottom()) - y }
    })
}

/// Empty space between two rectangles (0 when they touch or overlap).
fn gap(a: Area2, b: Area2) -> f32 {
    let dx = (a.x - b.right()).max(b.x - a.right()).max(0.0);
    let dy = (a.y - b.bottom()).max(b.y - a.bottom()).max(0.0);
    dx.hypot(dy)
}

impl<M> Arrangement<'_, M> {
    fn screens_with_placeholder(&self) -> BTreeMap<DeviceId, Vec<Rect>> {
        let mut s: BTreeMap<DeviceId, Vec<Rect>> =
            self.screens.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.clone(), v.clone())).collect();
        s.entry(self.me.to_owned()).or_insert_with(|| vec![PLACEHOLDER]);
        s
    }

    fn world_for(&self, shared: &Shared) -> World {
        World::build(shared, self.proportional, &self.me.to_owned(), &self.screens_with_placeholder(), self.new_side)
    }

    fn view(&self, size: Size) -> View {
        let world = self.world_for(self.shared);
        let all = union(world.devices.iter().map(|d| d.world_bounds())).unwrap_or(Area2 { x: 0.0, y: 0.0, w: 1.0, h: 1.0 });
        let lone = world.devices.len() < 2;
        let fill = if lone { (0.5, 0.6) } else { (0.88, 0.72) };
        let scale = ((size.width * fill.0) / all.w).min((size.height * fill.1) / all.h);
        let origin = Point::new(
            size.width / 2.0 - (all.x + all.w / 2.0) * scale,
            size.height / 2.0 - (all.y + all.h / 2.0) * scale + 8.0,
        );
        View { world, origin, scale }
    }

    /// The arrangement with `moving` dropped with its top-left at canvas
    /// point `top_left`.
    fn dropped(&self, view: &View, moving: &str, top_left: Point) -> Shared {
        let world = &view.world;
        let mut shared = self.shared.clone();
        let Some(m) = world.device(moving) else { return shared };
        let (x, y) = view.to_world(top_left);
        let mover = Area2 { x, y, ..m.world_bounds() };

        if self.proportional {
            // Order by where each device's center is, the dropped one included.
            let mut order: Vec<(f32, DeviceId)> = world
                .devices
                .iter()
                .map(|d| {
                    let b = if d.id == moving { mover } else { d.world_bounds() };
                    (b.x + b.w / 2.0, d.id.clone())
                })
                .collect();
            order.sort_by(|a, b| a.0.total_cmp(&b.0));
            shared.row = order.into_iter().map(|(_, id)| id).collect();
            return shared;
        }

        // Exact: snap to the nearest other device's edge.
        let Some(other) = world
            .devices
            .iter()
            .filter(|d| d.id != moving)
            .min_by(|a, b| gap(mover, a.world_bounds()).total_cmp(&gap(mover, b.world_bounds())))
        else {
            return shared;
        };
        let ob = other.world_bounds();
        let dx = (mover.x + mover.w / 2.0 - (ob.x + ob.w / 2.0)) / ((ob.w + mover.w) / 2.0);
        let dy = (mover.y + mover.h / 2.0 - (ob.y + ob.h / 2.0)) / ((ob.h + mover.h) / 2.0);
        let side = if dx.abs() >= dy.abs() {
            if dx >= 0.0 { Side::Right } else { Side::Left }
        } else if dy >= 0.0 {
            Side::Bottom
        } else {
            Side::Top
        };
        let (start, ol, tl) = if side.is_horizontal() { (mover.y - ob.y, ob.h, mover.h) } else { (mover.x - ob.x, ob.w, mover.w) };
        let snap = ol.max(tl) * SNAP;
        let offset = [(ol - tl) / 2.0, 0.0, ol - tl].into_iter().find(|at| (start - at).abs() <= snap).unwrap_or(start);
        let min = MIN_OVERLAP.min(ol).min(tl);
        let offset = offset.clamp(min - tl, ol - min);
        let origin = match side {
            Side::Right => (ob.right(), ob.y + offset),
            Side::Left => (ob.x - mover.w, ob.y + offset),
            Side::Bottom => (ob.x + offset, ob.bottom()),
            Side::Top => (ob.x + offset, ob.y - mover.h),
        };
        // Pin every device where it is now, so the layout is explicit.
        for d in &world.devices {
            shared.origins.insert(d.id.clone(), (d.origin.0.round() as i32, d.origin.1.round() as i32));
        }
        shared.origins.insert(moving.to_owned(), (origin.0.round() as i32, origin.1.round() as i32));
        shared
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

/// Where two devices' bounds touch, as a canvas line.
fn touching(a: Rectangle, b: Rectangle) -> Option<(Point, Point)> {
    const EPS: f32 = 1.5;
    let v = (a.y.max(b.y), (a.y + a.height).min(b.y + b.height));
    let h = (a.x.max(b.x), (a.x + a.width).min(b.x + b.width));
    if ((a.x + a.width) - b.x).abs() < EPS && v.1 > v.0 {
        return Some((Point::new(b.x, v.0), Point::new(b.x, v.1)));
    }
    if ((b.x + b.width) - a.x).abs() < EPS && v.1 > v.0 {
        return Some((Point::new(a.x, v.0), Point::new(a.x, v.1)));
    }
    if ((a.y + a.height) - b.y).abs() < EPS && h.1 > h.0 {
        return Some((Point::new(h.0, b.y), Point::new(h.1, b.y)));
    }
    if ((b.y + b.height) - a.y).abs() < EPS && h.1 > h.0 {
        return Some((Point::new(h.0, a.y), Point::new(h.1, a.y)));
    }
    None
}

impl<M> canvas::Program<M> for Arrangement<'_, M> {
    type State = State;

    fn update(&self, state: &mut State, event: &Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<Action<M>> {
        let on_change = self.on_change?;
        let view = self.view(bounds.size());
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let at = cursor.position_in(bounds)?;
                let device = view.device_at(at).filter(|d| d.id != self.me)?;
                let top_left = view.to_canvas(device.world_bounds()).position();
                state.grab = Some((device.id.clone(), at - top_left));
                state.drag_at = Some(top_left);
                Some(Action::request_redraw().and_capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let (_, grab) = state.grab.as_ref()?;
                let at = cursor.position_in(bounds).or_else(|| cursor.position().map(|p| Point::new(p.x - bounds.x, p.y - bounds.y)))?;
                state.drag_at = Some(at - *grab);
                Some(Action::request_redraw().and_capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let (id, _) = state.grab.take()?;
                let top_left = state.drag_at.take()?;
                Some(Action::publish(on_change(self.dropped(&view, &id, top_left))).and_capture())
            }
            _ => None,
        }
    }

    fn draw(&self, state: &State, renderer: &Renderer, theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let view = self.view(bounds.size());
        let mut frame = Frame::new(renderer, bounds.size());
        let text = palette.background.base.text;
        let muted = palette.background.strong.color;

        // While dragging: where it will land.
        let target = match (&state.grab, state.drag_at) {
            (Some((id, _)), Some(at)) => Some((id.clone(), self.world_for(&self.dropped(&view, id, at)))),
            _ => None,
        };
        let shown = target.as_ref().map_or(&view.world, |(_, w)| w);

        // Crossing lines between touching devices.
        for (i, a) in shown.devices.iter().enumerate() {
            for b in &shown.devices[i + 1..] {
                if let Some((p, q)) = touching(view.to_canvas(a.world_bounds()), view.to_canvas(b.world_bounds())) {
                    frame.stroke(&Path::line(p, q), Stroke::default().with_color(palette.success.base.color).with_width(4.0));
                }
            }
        }
        if let Some((id, world)) = &target
            && let Some(d) = world.device(id)
        {
            let r = view.to_canvas(d.world_bounds());
            let mut stroke = Stroke::default().with_color(palette.success.strong.color).with_width(2.0);
            stroke.line_dash = LineDash { segments: &[8.0, 6.0], offset: 0 };
            frame.stroke(&Path::rounded_rectangle(r.position(), r.size(), 6.0.into()), stroke);
        }

        for d in &view.world.devices {
            let me = d.id == self.me;
            let (fill, border, label_color) = if me {
                (palette.primary.weak.color, palette.primary.strong.color, palette.primary.weak.text)
            } else if self.connected.contains(&d.id) {
                (palette.success.weak.color, palette.success.strong.color, palette.success.weak.text)
            } else {
                (palette.background.weak.color, muted, palette.background.weak.text)
            };
            // The dragged device follows the cursor.
            let shift = match (&state.grab, state.drag_at) {
                (Some((id, _)), Some(at)) if *id == d.id => at - view.to_canvas(d.world_bounds()).position(),
                _ => Vector::ZERO,
            };
            let known = self.screens.get(&d.id).is_some_and(|v| !v.is_empty());
            for (area, s) in d.areas().into_iter().zip(&d.screens) {
                let label = if known { format!("{} × {}", s.w, s.h) } else { "Unknown".to_owned() };
                screen(&mut frame, view.to_canvas(area) + shift, fill, border, &label, label_color);
            }
            let name = self.names.get(&d.id).cloned().unwrap_or_default();
            let name = if !me && !self.connected.contains(&d.id) { format!("{name} · offline") } else { name };
            caption(&mut frame, view.to_canvas(d.world_bounds()) + shift, &name, text);
        }

        if view.world.devices.len() < 2 {
            let y = view.world.device(self.me).map_or(bounds.height / 2.0, |d| {
                let r = view.to_canvas(d.world_bounds());
                r.y + r.height + 22.0
            });
            frame.fill_text(Text {
                content: "Pair a device to arrange its screens.".into(),
                position: Point::new(bounds.width / 2.0, y),
                color: muted,
                size: 13.0.into(),
                align_x: alignment::Horizontal::Center.into(),
                align_y: alignment::Vertical::Top,
                ..Text::default()
            });
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if self.on_change.is_none() {
            return mouse::Interaction::default();
        }
        if state.grab.is_some() {
            return mouse::Interaction::Grabbing;
        }
        let view = self.view(bounds.size());
        match cursor.position_in(bounds).and_then(|at| view.device_at(at)) {
            Some(d) if d.id != self.me => mouse::Interaction::Grab,
            _ => mouse::Interaction::default(),
        }
    }
}
