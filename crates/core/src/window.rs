//! Window sharing: the lossless tile codec and the receiver's surface.
//!
//! The sender compares each captured frame with the previous one in 64×64
//! tiles and sends only the tiles that changed, LZ4-compressed. Nothing is
//! scaled or re-encoded, so text arrives pixel-exact. A static window costs
//! nothing; typing in an editor sends a few small tiles per keystroke.

use std::sync::Mutex;

use dextr_proto::{FrameUpdate, Tile};

pub const TILE: u32 = 64;

/// A captured frame in BGRA (or BGRx) byte order.
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row in `data`.
    pub stride: usize,
    pub data: &'a [u8],
}

/// Keeps the last frame sent and turns new frames into tile updates.
#[derive(Default)]
pub struct Encoder {
    width: u32,
    height: u32,
    /// The last frame sent, tightly packed (width × 4 bytes per row).
    prev: Vec<u8>,
}

impl Encoder {
    /// The tiles that differ from the last frame, or `None` if nothing
    /// changed. A new size sends everything.
    pub fn encode(&mut self, frame: &Frame<'_>) -> Option<FrameUpdate> {
        let (w, h) = (frame.width, frame.height);
        let row = w as usize * 4;
        if w == 0 || h == 0 || frame.stride < row || frame.data.len() < frame.stride * (h as usize - 1) + row {
            return None;
        }
        let resized = (w, h) != (self.width, self.height);
        if resized {
            self.width = w;
            self.height = h;
            self.prev = vec![0; row * h as usize];
        }

        let mut tiles = Vec::new();
        for ty in (0..h).step_by(TILE as usize) {
            for tx in (0..w).step_by(TILE as usize) {
                let (tw, th) = (TILE.min(w - tx), TILE.min(h - ty));
                let (x0, x1) = (tx as usize * 4, (tx + tw) as usize * 4);
                let src_row = |y: u32| {
                    let at = y as usize * frame.stride;
                    &frame.data[at + x0..at + x1]
                };
                let changed = resized || (ty..ty + th).any(|y| src_row(y) != &self.prev[y as usize * row + x0..y as usize * row + x1]);
                if !changed {
                    continue;
                }
                let mut pixels = Vec::with_capacity(tw as usize * th as usize * 4);
                for y in ty..ty + th {
                    let src = src_row(y);
                    pixels.extend_from_slice(src);
                    self.prev[y as usize * row + x0..y as usize * row + x1].copy_from_slice(src);
                }
                tiles.push(Tile { x: tx, y: ty, w: tw, h: th, data: lz4_flex::compress_prepend_size(&pixels) });
            }
        }
        (!tiles.is_empty()).then_some(FrameUpdate { width: w, height: h, tiles })
    }
}

/// A changed area of a [`Surface`], in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dirty {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// The pixels of a shared window on the receiving side, written by the
/// network and read by the renderer, which uploads only what changed.
#[derive(Default)]
pub struct Surface {
    inner: Mutex<SurfaceData>,
}

#[derive(Default)]
pub struct SurfaceData {
    pub width: u32,
    pub height: u32,
    /// BGRA, tightly packed.
    pub pixels: Vec<u8>,
    /// Areas changed since the renderer last took them.
    pub dirty: Vec<Dirty>,
    /// Everything changed (new size).
    pub full: bool,
    /// Bumped on every update.
    pub generation: u64,
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h) = self.size();
        write!(f, "Surface({w}×{h})")
    }
}

impl Surface {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, SurfaceData> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn size(&self) -> (u32, u32) {
        let d = self.lock();
        (d.width, d.height)
    }

    /// Applies an update. Returns whether the size changed.
    pub fn apply(&self, update: &FrameUpdate) -> anyhow::Result<bool> {
        let mut d = self.lock();
        let (w, h) = (update.width, update.height);
        anyhow::ensure!(w > 0 && h > 0 && (w as u64 * h as u64) <= 16384 * 16384, "bad frame size {w}×{h}");
        let resized = (w, h) != (d.width, d.height);
        if resized {
            d.width = w;
            d.height = h;
            d.pixels = vec![0; w as usize * h as usize * 4];
            d.dirty.clear();
            d.full = true;
        }
        let row = w as usize * 4;
        for t in &update.tiles {
            anyhow::ensure!(t.w > 0 && t.h > 0 && t.x + t.w <= w && t.y + t.h <= h, "tile outside the frame");
            let pixels = lz4_flex::decompress_size_prepended(&t.data)?;
            let trow = t.w as usize * 4;
            anyhow::ensure!(pixels.len() == trow * t.h as usize, "tile has the wrong size");
            for (i, src) in pixels.chunks_exact(trow).enumerate() {
                let at = (t.y as usize + i) * row + t.x as usize * 4;
                d.pixels[at..at + trow].copy_from_slice(src);
            }
            if !d.full {
                d.dirty.push(Dirty { x: t.x, y: t.y, w: t.w, h: t.h });
            }
        }
        d.generation += 1;
        Ok(resized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, stride: usize, seed: u8) -> Vec<u8> {
        let mut data = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize * 4 {
                data[y * stride + x] = (x as u8).wrapping_mul(7) ^ (y as u8).wrapping_add(seed);
            }
        }
        data
    }

    fn packed(data: &[u8], w: u32, h: u32, stride: usize) -> Vec<u8> {
        (0..h as usize).flat_map(|y| data[y * stride..y * stride + w as usize * 4].to_vec()).collect()
    }

    #[test]
    fn round_trip_sends_only_changed_tiles() {
        let (w, h, stride) = (200, 130, 200 * 4 + 32);
        let mut data = frame(w, h, stride, 1);
        let (mut enc, surface) = (Encoder::default(), Surface::default());

        let first = enc.encode(&Frame { width: w, height: h, stride, data: &data }).unwrap();
        assert_eq!(first.tiles.len(), 4 * 3);
        assert!(surface.apply(&first).unwrap());
        assert_eq!(surface.lock().pixels, packed(&data, w, h, stride));
        // The renderer uploads everything and takes the flag.
        surface.lock().full = false;

        // Nothing changed: nothing to send.
        assert!(enc.encode(&Frame { width: w, height: h, stride, data: &data }).is_none());

        // One pixel in the bottom-right tile.
        data[129 * stride + 199 * 4] ^= 0xFF;
        let update = enc.encode(&Frame { width: w, height: h, stride, data: &data }).unwrap();
        assert_eq!(update.tiles.len(), 1);
        assert_eq!((update.tiles[0].x, update.tiles[0].y, update.tiles[0].w, update.tiles[0].h), (192, 128, 8, 2));
        assert!(!surface.apply(&update).unwrap());
        let s = surface.lock();
        assert_eq!(s.pixels, packed(&data, w, h, stride));
        assert_eq!(s.dirty.last(), Some(&Dirty { x: 192, y: 128, w: 8, h: 2 }));
    }

    #[test]
    fn rejects_tiles_outside_the_frame() {
        let surface = Surface::default();
        let bad = FrameUpdate {
            width: 10,
            height: 10,
            tiles: vec![Tile { x: 8, y: 0, w: 4, h: 1, data: lz4_flex::compress_prepend_size(&[0; 16]) }],
        };
        assert!(surface.apply(&bad).is_err());
    }
}
