//! Pointer hand-off geometry, kept free of I/O so it can be tested.
//!
//! While this device controls the peer, it tracks a *virtual pointer* in the
//! peer's coordinates and sends it as absolute positions. Because the
//! controller decides where the pointer is, both sides always agree on it and
//! the return edge can be detected without asking the peer.

use dextr_proto::{Rect, Side};

/// How the peer's screens are placed against ours: which side, and how far
/// along that side. Positions are in pixels, like the OS display settings,
/// so the pointer only crosses where the two edges actually touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub side: Side,
    /// Start of the peer's edge relative to the start of ours (top for
    /// left/right, left for top/bottom). `None` centers it.
    pub offset: Option<i32>,
}

/// How much of the edges must always touch.
pub const MIN_OVERLAP: i32 = 64;

/// Lengths of our edge and theirs along the shared side.
fn lengths(side: Side, ob: Rect, tb: Rect) -> (i32, i32) {
    if side.is_horizontal() { (ob.h as i32, tb.h as i32) } else { (ob.w as i32, tb.w as i32) }
}

impl Link {
    /// The offset in use, kept within range so the edges always touch.
    pub fn offset_for(&self, ob: Rect, tb: Rect) -> i32 {
        let (ol, tl) = lengths(self.side, ob, tb);
        let min = MIN_OVERLAP.min(ol).min(tl);
        self.offset.unwrap_or((ol - tl) / 2).clamp(min - tl, ol - min)
    }

    /// Top-left of the peer's screens in our coordinates.
    pub fn place(&self, ob: Rect, tb: Rect) -> (i32, i32) {
        let off = self.offset_for(ob, tb);
        match self.side {
            Side::Right => (ob.x + ob.w as i32, ob.y + off),
            Side::Left => (ob.x - tb.w as i32, ob.y + off),
            Side::Bottom => (ob.x + off, ob.y + ob.h as i32),
            Side::Top => (ob.x + off, ob.y - tb.h as i32),
        }
    }

    /// What to add to a peer coordinate to get ours (along x, along y).
    fn shift(&self, ob: Rect, tb: Rect) -> (f32, f32) {
        let (px, py) = self.place(ob, tb);
        ((px - tb.x) as f32, (py - tb.y) as f32)
    }
}

/// Position along the shared edge.
fn along(side: Side, x: f32, y: f32) -> f32 {
    if side.is_horizontal() { y } else { x }
}

/// Start and end of a rectangle along the shared edge.
fn span(side: Side, r: Rect) -> (f32, f32) {
    if side.is_horizontal() { (r.y as f32, r.bottom()) } else { (r.x as f32, r.right()) }
}

/// Where the pointer appears on the peer when it leaves us at (x, y), or
/// `None` if the peer's screens don't touch our edge there.
pub fn entry_point(link: Link, x: f32, y: f32, ours: &[Rect], theirs: &[Rect]) -> Option<(f32, f32)> {
    let (ob, tb) = (Rect::bounds(ours)?, Rect::bounds(theirs)?);
    let (sx, sy) = link.shift(ob, tb);
    let a = along(link.side, x, y) - if link.side.is_horizontal() { sy } else { sx };
    let (start, end) = span(link.side, tb);
    if a < start || a >= end {
        return None;
    }
    let p = match link.side {
        Side::Right => (tb.x as f32 + 1.0, a),
        Side::Left => (tb.right() - 2.0, a),
        Side::Bottom => (a, tb.y as f32 + 1.0),
        Side::Top => (a, tb.bottom() - 2.0),
    };
    Some(Rect::clamp_to_any(theirs, p.0, p.1))
}

/// Whether (x, y) on our edge is within `size` pixels of either end of it.
pub fn in_dead_corner(side: Side, x: f32, y: f32, ours: &[Rect], size: f32) -> bool {
    if size <= 0.0 {
        return false;
    }
    let Some(b) = Rect::bounds(ours) else { return false };
    let pos = along(side, x, y);
    let (start, end) = span(side, b);
    pos - start < size || end - pos <= size
}

/// Whether the virtual pointer on the peer has crossed back over the part
/// of its edge that touches ours. Elsewhere the edge is a wall.
pub fn crossed_back(link: Link, x: f32, y: f32, ours: &[Rect], theirs: &[Rect]) -> bool {
    let (Some(ob), Some(tb)) = (Rect::bounds(ours), Rect::bounds(theirs)) else { return true };
    let past = match link.side {
        Side::Right => x < tb.x as f32,
        Side::Left => x >= tb.right(),
        Side::Bottom => y < tb.y as f32,
        Side::Top => y >= tb.bottom(),
    };
    let (sx, sy) = link.shift(ob, tb);
    let a = along(link.side, x, y) + if link.side.is_horizontal() { sy } else { sx };
    let (start, end) = span(link.side, ob);
    past && a >= start && a < end
}

/// Where our pointer reappears when it comes back from the peer at (x, y),
/// placed just inside our edge so it doesn't immediately re-trigger it.
pub fn return_point(link: Link, x: f32, y: f32, ours: &[Rect], theirs: &[Rect]) -> Option<(f32, f32)> {
    let (ob, tb) = (Rect::bounds(ours)?, Rect::bounds(theirs)?);
    let (sx, sy) = link.shift(ob, tb);
    inside_edge(link.side, x + sx, y + sy, ours)
}

/// (x, y) moved just inside our edge on `side`.
pub fn inside_edge(side: Side, x: f32, y: f32, ours: &[Rect]) -> Option<(f32, f32)> {
    let ob = Rect::bounds(ours)?;
    let p = match side {
        Side::Right => (ob.right() - 2.0, y),
        Side::Left => (ob.x as f32 + 1.0, y),
        Side::Bottom => (x, ob.bottom() - 2.0),
        Side::Top => (x, ob.y as f32 + 1.0),
    };
    Some(Rect::clamp_to_any(ours, p.0, p.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAPTOP: [Rect; 1] = [Rect::new(0, 0, 1920, 1080)];
    const MAC: [Rect; 1] = [Rect::new(0, 0, 1512, 982)];

    fn centered(side: Side) -> Link {
        Link { side, offset: None }
    }

    #[test]
    fn centered_right_round_trip() {
        let link = centered(Side::Right);
        // Centered: the Mac's top is 49 px below the laptop's.
        let (x, y) = entry_point(link, 1919.0, 540.0, &LAPTOP, &MAC).unwrap();
        assert_eq!((x, y), (1.0, 491.0));
        assert!(!crossed_back(link, x, y, &LAPTOP, &MAC));
        assert!(crossed_back(link, -1.0, y, &LAPTOP, &MAC));
        assert_eq!(return_point(link, -1.0, y, &LAPTOP, &MAC).unwrap(), (1918.0, 540.0));
    }

    #[test]
    fn top_edge() {
        let link = centered(Side::Top);
        let (x, y) = entry_point(link, 960.0, 0.0, &LAPTOP, &MAC).unwrap();
        assert_eq!((x, y), (756.0, 980.0));
        assert!(crossed_back(link, x, 982.0, &LAPTOP, &MAC));
        assert_eq!(return_point(link, x, 982.0, &LAPTOP, &MAC).unwrap(), (960.0, 1.0));
    }

    #[test]
    fn crossing_only_where_edges_touch() {
        // The Mac sits low: its top is level with the laptop's y = 600.
        let link = Link { side: Side::Right, offset: Some(600) };
        assert!(entry_point(link, 1919.0, 300.0, &LAPTOP, &MAC).is_none());
        assert_eq!(entry_point(link, 1919.0, 700.0, &LAPTOP, &MAC).unwrap(), (1.0, 100.0));
        // Below the laptop's bottom the Mac's left edge is a wall…
        assert!(!crossed_back(link, -1.0, 600.0, &LAPTOP, &MAC));
        // …above it the pointer comes back at the matching height.
        assert!(crossed_back(link, -1.0, 400.0, &LAPTOP, &MAC));
        assert_eq!(return_point(link, -1.0, 400.0, &LAPTOP, &MAC).unwrap(), (1918.0, 1000.0));
    }

    #[test]
    fn offset_keeps_edges_touching() {
        let ob = LAPTOP[0];
        let tb = MAC[0];
        assert_eq!(Link { side: Side::Right, offset: Some(5000) }.offset_for(ob, tb), 1080 - MIN_OVERLAP);
        assert_eq!(Link { side: Side::Right, offset: Some(-5000) }.offset_for(ob, tb), MIN_OVERLAP - 982);
        assert_eq!(Link { side: Side::Left, offset: None }.place(ob, tb), (-1512, 49));
    }

    #[test]
    fn dead_corners() {
        assert!(in_dead_corner(Side::Right, 1919.0, 5.0, &LAPTOP, 16.0));
        assert!(in_dead_corner(Side::Right, 1919.0, 1070.0, &LAPTOP, 16.0));
        assert!(!in_dead_corner(Side::Right, 1919.0, 540.0, &LAPTOP, 16.0));
        assert!(!in_dead_corner(Side::Right, 1919.0, 0.0, &LAPTOP, 0.0));
    }

    #[test]
    fn no_peer_screens() {
        let link = centered(Side::Right);
        assert!(entry_point(link, 0.0, 0.0, &LAPTOP, &[]).is_none());
        assert!(crossed_back(link, 0.0, 0.0, &LAPTOP, &[]));
    }
}
