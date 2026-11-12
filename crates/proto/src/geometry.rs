use serde::{Deserialize, Serialize};

/// Where a peer sits relative to this device.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Side {
    Left,
    #[default]
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    pub const fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    pub const fn is_horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::Left => "Left",
            Side::Right => "Right",
            Side::Top => "Top",
            Side::Bottom => "Bottom",
        })
    }
}

/// A screen in logical pixels.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        (self.x as i64 + self.w as i64) as f32
    }

    pub fn bottom(&self) -> f32 {
        (self.y as i64 + self.h as i64) as f32
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x as f32 && x < self.right() && y >= self.y as f32 && y < self.bottom()
    }

    /// The nearest point inside this rect.
    pub fn clamp(&self, x: f32, y: f32) -> (f32, f32) {
        (
            x.clamp(self.x as f32, self.right() - 1.0),
            y.clamp(self.y as f32, self.bottom() - 1.0),
        )
    }

    /// Bounding box of all screens.
    pub fn bounds(rects: &[Rect]) -> Option<Rect> {
        let first = rects.first()?;
        let (mut x0, mut y0) = (first.x as i64, first.y as i64);
        let (mut x1, mut y1) = (x0 + first.w as i64, y0 + first.h as i64);
        for r in &rects[1..] {
            x0 = x0.min(r.x as i64);
            y0 = y0.min(r.y as i64);
            x1 = x1.max(r.x as i64 + r.w as i64);
            y1 = y1.max(r.y as i64 + r.h as i64);
        }
        Some(Rect::new(x0 as i32, y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    /// Clamps a point into the nearest of several screens, so the pointer never
    /// sits in a gap of a non-rectangular multi-monitor layout.
    pub fn clamp_to_any(rects: &[Rect], x: f32, y: f32) -> (f32, f32) {
        if rects.iter().any(|r| r.contains(x, y)) {
            return (x, y);
        }
        rects
            .iter()
            .map(|r| r.clamp(x, y))
            .min_by(|a, b| {
                let da = (a.0 - x).powi(2) + (a.1 - y).powi(2);
                let db = (b.0 - x).powi(2) + (b.1 - y).powi(2);
                da.total_cmp(&db)
            })
            .unwrap_or((x, y))
    }

    /// Screens that touch the outer edge of the layout on `side`.
    pub fn on_outer_edge(rects: &[Rect], side: Side) -> Vec<Rect> {
        let Some(b) = Rect::bounds(rects) else {
            return Vec::new();
        };
        rects
            .iter()
            .copied()
            .filter(|r| match side {
                Side::Left => r.x == b.x,
                Side::Right => r.right() == b.right(),
                Side::Top => r.y == b.y,
                Side::Bottom => r.bottom() == b.bottom(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_into_l_shaped_layout() {
        // 1920x1080 at origin, a smaller 1280x720 to its right, top-aligned.
        let screens = [Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 1280, 720)];
        // A point below the small screen is pulled up onto it, not left into the gap.
        assert_eq!(Rect::clamp_to_any(&screens, 2500.0, 900.0), (2500.0, 719.0));
        assert_eq!(Rect::bounds(&screens), Some(Rect::new(0, 0, 3200, 1080)));
        assert_eq!(Rect::on_outer_edge(&screens, Side::Right), vec![screens[1]]);
    }
}
