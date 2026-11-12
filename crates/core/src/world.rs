//! The shared layout of every paired device's screens.
//!
//! All devices are placed in one space. The device whose mouse is in use
//! tracks the pointer in that space and sends it to whichever device's
//! screen it is over, so the pointer can go from A to B and on to C.
//!
//! * **Exact**: each device's screens keep their real size (in each device's
//!   own logical pixels) and sit where they were dragged. The pointer crosses
//!   only where screens touch, at the same spot.
//! * **Proportional**: the devices sit in one row, each scaled to the same
//!   height, so whole edges meet whole edges whatever the sizes.

use std::collections::BTreeMap;

use dextr_proto::{Rect, Side};
use serde::{Deserialize, Serialize};

/// A device, by its certificate fingerprint as text.
pub type DeviceId = String;

/// Where the devices are, shared by all of them (the newest wins).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Arrangement {
    /// Exact: the top-left of each device's screens in the shared space.
    pub origins: BTreeMap<DeviceId, (i32, i32)>,
    /// Proportional: the devices, left to right.
    pub row: Vec<DeviceId>,
    /// When it last changed (Unix ms).
    pub changed_at: u64,
}

/// A rectangle in the shared space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area2 {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Area2 {
    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    fn clamp(&self, (x, y): (f32, f32)) -> (f32, f32) {
        (x.clamp(self.x, self.right() - 0.01), y.clamp(self.y, self.bottom() - 0.01))
    }

    fn distance(&self, p: (f32, f32)) -> f32 {
        let c = self.clamp(p);
        (c.0 - p.0).hypot(c.1 - p.1)
    }
}

/// One device in the shared space.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub id: DeviceId,
    /// Its screens, in its own logical pixels.
    pub screens: Vec<Rect>,
    /// Their bounds, in its own logical pixels.
    pub bounds: Rect,
    /// Where the bounds' top-left sits in the shared space.
    pub origin: (f32, f32),
    /// Shared-space units per pixel of this device.
    pub scale: f32,
}

impl Device {
    pub fn to_world(&self, (x, y): (f32, f32)) -> (f32, f32) {
        (self.origin.0 + (x - self.bounds.x as f32) * self.scale, self.origin.1 + (y - self.bounds.y as f32) * self.scale)
    }

    pub fn to_local(&self, (x, y): (f32, f32)) -> (f32, f32) {
        ((x - self.origin.0) / self.scale + self.bounds.x as f32, (y - self.origin.1) / self.scale + self.bounds.y as f32)
    }

    /// Its screens in the shared space.
    pub fn areas(&self) -> Vec<Area2> {
        self.screens
            .iter()
            .map(|r| {
                let (x, y) = self.to_world((r.x as f32, r.y as f32));
                Area2 { x, y, w: r.w as f32 * self.scale, h: r.h as f32 * self.scale }
            })
            .collect()
    }

    pub fn world_bounds(&self) -> Area2 {
        Area2 { x: self.origin.0, y: self.origin.1, w: self.bounds.w as f32 * self.scale, h: self.bounds.h as f32 * self.scale }
    }

    pub fn contains(&self, p: (f32, f32)) -> bool {
        self.areas().iter().any(|a| a.contains(p))
    }

    /// The nearest point on its screens.
    pub fn clamp(&self, p: (f32, f32)) -> (f32, f32) {
        let areas = self.areas();
        if areas.iter().any(|a| a.contains(p)) {
            return p;
        }
        areas
            .iter()
            .min_by(|a, b| a.distance(p).total_cmp(&b.distance(p)))
            .map(|a| a.clamp(p))
            .unwrap_or(p)
    }
}

/// Height of every device in the Proportional row.
const ROW_HEIGHT: f32 = 1000.0;
/// How far past an edge to look for the next device, in shared units.
const NUDGE: f32 = 2.0;

/// Every known device placed in the shared space.
#[derive(Clone, Debug, PartialEq)]
pub struct World {
    pub devices: Vec<Device>,
    pub proportional: bool,
}

impl World {
    /// Places the devices with known screens. Devices not in the arrangement
    /// yet go next to the others: on `new_side` of this device for the first,
    /// then further out.
    pub fn build(
        arrangement: &Arrangement,
        proportional: bool,
        me: &DeviceId,
        screens: &BTreeMap<DeviceId, Vec<Rect>>,
        new_side: Side,
    ) -> World {
        let known: Vec<(&DeviceId, &Vec<Rect>)> = screens.iter().filter(|(_, s)| !s.is_empty()).collect();
        let bounds = |s: &[Rect]| Rect::bounds(s).unwrap();
        let mut devices = Vec::new();

        if proportional {
            // Left to right: the arranged order, then newcomers.
            let mut order: Vec<&DeviceId> = arrangement.row.iter().filter(|id| screens.get(*id).is_some_and(|s| !s.is_empty())).collect();
            for (id, _) in &known {
                if !order.contains(id) {
                    if *id == me {
                        order.insert(0, id);
                    } else if new_side == Side::Left {
                        let at = order.iter().position(|i| *i == me).unwrap_or(0);
                        order.insert(at, id);
                    } else {
                        order.push(id);
                    }
                }
            }
            let mut x = 0.0;
            for id in order {
                let s = &screens[id];
                let b = bounds(s);
                let scale = ROW_HEIGHT / b.h.max(1) as f32;
                devices.push(Device { id: id.clone(), screens: s.clone(), bounds: b, origin: (x, 0.0), scale });
                x += b.w as f32 * scale;
            }
            return World { devices, proportional };
        }

        // Exact: arranged origins; this device defaults to its own coordinates.
        for (id, s) in &known {
            let b = bounds(s);
            let origin = match arrangement.origins.get(*id) {
                Some(&(x, y)) => (x as f32, y as f32),
                None if *id == me => (b.x as f32, b.y as f32),
                None => continue,
            };
            devices.push(Device { id: (*id).clone(), screens: (*s).clone(), bounds: b, origin, scale: 1.0 });
        }
        // Newcomers: next to the devices already placed, centered on this one.
        for (id, s) in &known {
            if devices.iter().any(|d| &d.id == *id) {
                continue;
            }
            let b = bounds(s);
            let (w, h) = (b.w as f32, b.h as f32);
            let all = union(devices.iter().map(Device::world_bounds));
            let mine = devices.iter().find(|d| &d.id == me).map(Device::world_bounds).unwrap_or(all);
            let origin = match new_side {
                Side::Right => (all.right(), mine.y + (mine.h - h) / 2.0),
                Side::Left => (all.x - w, mine.y + (mine.h - h) / 2.0),
                Side::Bottom => (mine.x + (mine.w - w) / 2.0, all.bottom()),
                Side::Top => (mine.x + (mine.w - w) / 2.0, all.y - h),
            };
            let origin = (origin.0.round(), origin.1.round());
            devices.push(Device { id: (*id).clone(), screens: (*s).clone(), bounds: b, origin, scale: 1.0 });
        }
        World { devices, proportional }
    }

    pub fn device(&self, id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// The device whose screen contains `p`.
    pub fn device_at(&self, p: (f32, f32)) -> Option<&Device> {
        self.devices.iter().find(|d| d.contains(p))
    }

    /// The device just past `side` of `from` at its local point `local`, and
    /// where the pointer enters it (its local coordinates).
    pub fn beyond(&self, from: &str, local: (f32, f32), side: Side) -> Option<(&Device, (f32, f32))> {
        let me = self.device(from)?;
        let (x, y) = me.to_world(local);
        // Start from the edge itself, so a point a little inside still counts.
        let b = me.world_bounds();
        let p = match side {
            Side::Right => (b.right().max(x) + NUDGE - 1.0, y),
            Side::Left => (b.x.min(x) - NUDGE, y),
            Side::Bottom => (x, b.bottom().max(y) + NUDGE - 1.0),
            Side::Top => (x, b.y.min(y) - NUDGE),
        };
        let target = self.devices.iter().find(|d| d.id != from && d.contains(p))?;
        Some((target, target.to_local(p)))
    }

    /// Sides of `id` that have another device past them somewhere.
    pub fn sides_with_neighbors(&self, id: &str) -> Vec<Side> {
        let Some(me) = self.device(id) else { return Vec::new() };
        Side::ALL
            .iter()
            .copied()
            .filter(|&side| {
                Rect::on_outer_edge(&me.screens, side).iter().any(|r| {
                    (0..=32).any(|i| {
                        let f = i as f32 / 32.0;
                        let (x, y) = match side {
                            Side::Right => (r.right() - 1.0, r.y as f32 + f * (r.h as f32 - 1.0)),
                            Side::Left => (r.x as f32, r.y as f32 + f * (r.h as f32 - 1.0)),
                            Side::Bottom => (r.x as f32 + f * (r.w as f32 - 1.0), r.bottom() - 1.0),
                            Side::Top => (r.x as f32 + f * (r.w as f32 - 1.0), r.y as f32),
                        };
                        self.beyond(id, (x, y), side).is_some()
                    })
                })
            })
            .collect()
    }
}

fn union(areas: impl Iterator<Item = Area2>) -> Area2 {
    let mut out: Option<Area2> = None;
    for a in areas {
        out = Some(match out {
            None => a,
            Some(o) => {
                let (x, y) = (o.x.min(a.x), o.y.min(a.y));
                Area2 { x, y, w: o.right().max(a.right()) - x, h: o.bottom().max(a.bottom()) - y }
            }
        });
    }
    out.unwrap_or(Area2 { x: 0.0, y: 0.0, w: 0.0, h: 0.0 })
}

/// Which side of `screens` the point is on (the nearest outer edge among
/// `sides`), for a capture that reports only where the pointer hit.
pub fn side_at(screens: &[Rect], (x, y): (f32, f32), sides: &[Side]) -> Option<Side> {
    let b = Rect::bounds(screens)?;
    sides
        .iter()
        .copied()
        .min_by(|a, c| {
            let d = |s: Side| match s {
                Side::Right => (b.right() - 1.0 - x).abs(),
                Side::Left => (x - b.x as f32).abs(),
                Side::Bottom => (b.bottom() - 1.0 - y).abs(),
                Side::Top => (y - b.y as f32).abs(),
            };
            d(*a).total_cmp(&d(*c))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screens(list: &[(&str, &[Rect])]) -> BTreeMap<DeviceId, Vec<Rect>> {
        list.iter().map(|(id, s)| (id.to_string(), s.to_vec())).collect()
    }

    const LAPTOP: Rect = Rect::new(0, 0, 1920, 1080);
    const MAC: Rect = Rect::new(0, 0, 1512, 982);
    const DESK: Rect = Rect::new(0, 0, 2560, 1440);

    #[test]
    fn exact_chain_a_to_b_to_c() {
        // Laptop A, Mac B to its right, desktop C to the Mac's right.
        let arr = Arrangement {
            origins: [("a".into(), (0, 0)), ("b".into(), (1920, 0)), ("c".into(), (3432, 0))].into(),
            ..Default::default()
        };
        let s = screens(&[("a", &[LAPTOP]), ("b", &[MAC]), ("c", &[DESK])]);
        let w = World::build(&arr, false, &"a".into(), &s, Side::Right);
        // Leaving A's right edge at y = 500 enters B at its left edge.
        let (b, at) = w.beyond("a", (1919.0, 500.0), Side::Right).unwrap();
        assert_eq!(b.id, "b");
        assert!(at.0 < 2.0 && at.1 == 500.0, "{at:?}");
        // From B's right edge, on to C.
        let (c, at) = w.beyond("b", (1511.0, 500.0), Side::Right).unwrap();
        assert_eq!(c.id, "c");
        assert!(at.0 < 2.0 && at.1 == 500.0);
        // Below the Mac (y = 1000) A's right edge touches nothing: a wall.
        assert!(w.beyond("a", (1919.0, 1000.0), Side::Right).is_none());
        assert_eq!(w.sides_with_neighbors("a"), vec![Side::Right]);
        assert_eq!(w.sides_with_neighbors("b"), vec![Side::Left, Side::Right]);
    }

    #[test]
    fn proportional_row_maps_whole_edges() {
        let arr = Arrangement { row: vec!["a".into(), "b".into()], ..Default::default() };
        let s = screens(&[("a", &[LAPTOP]), ("b", &[MAC])]);
        let w = World::build(&arr, true, &"a".into(), &s, Side::Right);
        // 30% down A's right edge → 30% down B's left edge.
        let (b, at) = w.beyond("a", (1919.0, 324.0), Side::Right).unwrap();
        assert_eq!(b.id, "b");
        assert!((at.1 - 0.3 * 982.0).abs() < 1.0, "{at:?}");
        // And the whole edge: the bottom corner too.
        assert!(w.beyond("a", (1919.0, 1079.0), Side::Right).is_some());
        // Moving back across lands at the same fraction on A.
        let back = w.device("b").unwrap().to_world((0.0, 0.5 * 982.0));
        let a = w.device_at((back.0 - 3.0, back.1)).unwrap();
        assert_eq!(a.id, "a");
        assert!((a.to_local((back.0 - 3.0, back.1)).1 - 540.0).abs() < 1.0);
    }

    #[test]
    fn newcomers_are_placed_beside_this_device() {
        let s = screens(&[("a", &[LAPTOP]), ("b", &[MAC])]);
        let w = World::build(&Arrangement::default(), false, &"a".into(), &s, Side::Left);
        let b = w.device("b").unwrap();
        assert_eq!(b.origin, (-1512.0, 49.0));
        assert!(w.beyond("a", (0.0, 540.0), Side::Left).is_some());
        assert_eq!(side_at(&[LAPTOP], (0.0, 300.0), &[Side::Left, Side::Right]), Some(Side::Left));
    }

    #[test]
    fn clamp_keeps_the_pointer_on_a_device() {
        let s = screens(&[("a", &[LAPTOP])]);
        let w = World::build(&Arrangement::default(), false, &"a".into(), &s, Side::Right);
        let a = w.device("a").unwrap();
        assert_eq!(a.clamp((2500.0, -50.0)), (1919.99, 0.0));
    }
}
