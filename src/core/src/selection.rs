// SPDX-License-Identifier: GPL-3.0-or-later
//! 选区：画布上每个像素一个 0..=ONE 的"选中程度"，稀疏存储。
//!
//! 有选区时，绘制、清空、填充都只作用在选区里（规格 F-070 至 F-072）。

use crate::tile::{tiles_in, MaskMap, TileCoord, TILE};
use crate::{Rect, ONE};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Combine {
    Replace,
    Add,
    Subtract,
}

pub struct Selection {
    mask: MaskMap,
    /// 包住全部选中像素的矩形（不一定是最紧的）
    bounds: Rect,
}

impl Selection {
    pub fn empty() -> Self {
        Self { mask: MaskMap::default(), bounds: Rect::EMPTY }
    }

    /// 由覆盖率分块直接构成（填充、魔棒等算法用）。`bounds` 要包住全部非零像素。
    pub fn from_mask(mask: MaskMap, bounds: Rect) -> Self {
        Self { mask, bounds }
    }

    pub fn rect(r: Rect) -> Self {
        let mut s = Self::empty();
        for tc in tiles_in(r) {
            let t = s.mask.get_mut(tc);
            let (ox, oy) = (tc.0 * TILE, tc.1 * TILE);
            for y in r.y0.max(oy)..r.y1.min(oy + TILE) {
                for x in r.x0.max(ox)..r.x1.min(ox + TILE) {
                    t[((y - oy) * TILE + (x - ox)) as usize] = ONE as u16;
                }
            }
        }
        s.bounds = r;
        s
    }

    /// 套索：把一圈点围成的多边形内部选中（奇偶规则，按像素中心判定）。
    pub fn polygon(points: &[(f32, f32)], clip: Rect) -> Self {
        let mut s = Self::empty();
        if points.len() < 3 {
            return s;
        }
        let ys = points.iter().map(|p| p.1);
        let y0 = (ys.clone().fold(f32::MAX, f32::min).floor() as i32).max(clip.y0);
        let y1 = (ys.fold(f32::MIN, f32::max).ceil() as i32).min(clip.y1);
        let mut xs: Vec<f32> = Vec::new();
        for y in y0..y1 {
            let cy = y as f32 + 0.5;
            xs.clear();
            for i in 0..points.len() {
                let (a, b) = (points[i], points[(i + 1) % points.len()]);
                if (a.1 <= cy) != (b.1 <= cy) {
                    xs.push(a.0 + (cy - a.1) / (b.1 - a.1) * (b.0 - a.0));
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            for pair in xs.as_chunks::<2>().0 {
                // 像素中心落在 [左交点, 右交点) 里的算选中。
                let xa = ((pair[0] - 0.5).ceil() as i32).max(clip.x0);
                let xb = ((pair[1] - 0.5).ceil() as i32).min(clip.x1);
                for x in xa..xb {
                    s.set(x, y, ONE as u16);
                }
                if xb > xa {
                    s.bounds = s.bounds.union(Rect { x0: xa, y0: y, x1: xb, y1: y + 1 });
                }
            }
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.bounds.is_empty()
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn tile(&self, tc: TileCoord) -> Option<&[u16]> {
        self.mask.get(tc)
    }

    pub fn get(&self, x: i32, y: i32) -> u16 {
        let tc = (x.div_euclid(TILE), y.div_euclid(TILE));
        self.mask.get(tc).map_or(0, |t| t[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize])
    }

    fn set(&mut self, x: i32, y: i32, v: u16) {
        let tc = (x.div_euclid(TILE), y.div_euclid(TILE));
        self.mask.get_mut(tc)[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize] = v;
    }

    /// 反选：画布范围内，选中的变成不选，不选的变成选中。
    pub fn inverted(&self, canvas: Rect) -> Self {
        let mut s = Self::empty();
        for tc in tiles_in(canvas) {
            let src = self.mask.get(tc);
            let dst = s.mask.get_mut(tc);
            let (ox, oy) = (tc.0 * TILE, tc.1 * TILE);
            for y in canvas.y0.max(oy)..canvas.y1.min(oy + TILE) {
                for x in canvas.x0.max(ox)..canvas.x1.min(ox + TILE) {
                    let i = ((y - oy) * TILE + (x - ox)) as usize;
                    dst[i] = ONE as u16 - src.map_or(0, |t| t[i]);
                }
            }
        }
        s.bounds = canvas;
        s.trimmed()
    }

    /// 与另一个选区合并：相加，或者从自己里面减掉对方。
    pub fn combined(&self, other: &Selection, how: Combine) -> Self {
        let mut s = Self::empty();
        let area = match how {
            Combine::Replace => other.bounds,
            Combine::Add => self.bounds.union(other.bounds),
            Combine::Subtract => self.bounds,
        };
        for tc in tiles_in(area) {
            let (a, b) = (self.mask.get(tc), other.mask.get(tc));
            if a.is_none() && (b.is_none() || how == Combine::Subtract) {
                continue;
            }
            let dst = s.mask.get_mut(tc);
            for (i, d) in dst.iter_mut().enumerate() {
                let (va, vb) = (a.map_or(0, |t| t[i]), b.map_or(0, |t| t[i]));
                *d = match how {
                    Combine::Replace => vb,
                    Combine::Add => va.max(vb),
                    Combine::Subtract => va.min(ONE as u16 - vb),
                };
            }
        }
        s.bounds = area;
        s.trimmed()
    }

    /// 向外扩（`px` 为正）或向内缩（为负）若干像素。
    pub fn grown(&self, px: i32, canvas: Rect) -> Self {
        let mut cur = Self { mask: clone_mask(&self.mask, self.bounds), bounds: self.bounds };
        for _ in 0..px.unsigned_abs() {
            let area = if px > 0 {
                Rect { x0: cur.bounds.x0 - 1, y0: cur.bounds.y0 - 1, x1: cur.bounds.x1 + 1, y1: cur.bounds.y1 + 1 }.intersect(canvas)
            } else {
                cur.bounds
            };
            let mut next = Self::empty();
            for y in area.y0..area.y1 {
                for x in area.x0..area.x1 {
                    let around = [cur.get(x, y), cur.get(x - 1, y), cur.get(x + 1, y), cur.get(x, y - 1), cur.get(x, y + 1)];
                    let v = if px > 0 { around.into_iter().max() } else { around.into_iter().min() }.unwrap();
                    if v != 0 {
                        next.set(x, y, v);
                    }
                }
            }
            next.bounds = area;
            cur = next;
        }
        cur.trimmed()
    }

    /// 把外接矩形收紧到实际有内容的范围；全空时变成空选区。
    fn trimmed(mut self) -> Self {
        let mut tight = Rect::EMPTY;
        for tc in tiles_in(self.bounds) {
            let Some(t) = self.mask.get(tc) else { continue };
            let (ox, oy) = (tc.0 * TILE, tc.1 * TILE);
            for y in 0..TILE {
                for x in 0..TILE {
                    if t[(y * TILE + x) as usize] != 0 {
                        tight = tight.union(Rect { x0: ox + x, y0: oy + y, x1: ox + x + 1, y1: oy + y + 1 });
                    }
                }
            }
        }
        self.bounds = tight;
        self
    }
}

impl Clone for Selection {
    fn clone(&self) -> Self {
        Self { mask: clone_mask(&self.mask, self.bounds), bounds: self.bounds }
    }
}

fn clone_mask(m: &MaskMap, bounds: Rect) -> MaskMap {
    let mut out = MaskMap::default();
    for tc in tiles_in(bounds) {
        if let Some(t) = m.get(tc) {
            out.get_mut(tc).copy_from_slice(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANVAS: Rect = Rect { x0: 0, y0: 0, x1: 200, y1: 150 };

    fn count(s: &Selection) -> usize {
        let b = s.bounds();
        (b.y0..b.y1).flat_map(|y| (b.x0..b.x1).map(move |x| (x, y))).filter(|&(x, y)| s.get(x, y) != 0).count()
    }

    #[test]
    fn rectangle_selects_exactly_its_pixels() {
        let s = Selection::rect(Rect { x0: 60, y0: 10, x1: 70, y1: 80 });
        assert_eq!(count(&s), 10 * 70);
        assert_eq!(s.get(60, 10) as u32, ONE);
        assert_eq!(s.get(59, 10), 0);
        assert_eq!(s.get(69, 79) as u32, ONE);
        assert_eq!(s.get(70, 79), 0);
    }

    #[test]
    fn polygon_fills_a_triangle() {
        let s = Selection::polygon(&[(10.0, 10.0), (110.0, 10.0), (10.0, 110.0)], CANVAS);
        // 直角边长 100 的三角形，面积 5000。
        let n = count(&s) as i32;
        assert!((n - 5000).abs() < 120, "{n}");
        assert_ne!(s.get(20, 20), 0);
        assert_eq!(s.get(100, 100), 0);
        assert!(Selection::polygon(&[(1.0, 1.0), (5.0, 5.0)], CANVAS).is_empty());
    }

    #[test]
    fn invert_and_combine() {
        let a = Selection::rect(Rect { x0: 0, y0: 0, x1: 100, y1: 100 });
        let b = Selection::rect(Rect { x0: 50, y0: 50, x1: 150, y1: 150 });
        assert_eq!(count(&a.combined(&b, Combine::Add)), 100 * 100 * 2 - 50 * 50);
        assert_eq!(count(&a.combined(&b, Combine::Subtract)), 100 * 100 - 50 * 50);
        assert_eq!(count(&a.combined(&b, Combine::Replace)), 100 * 100);
        let inv = a.inverted(CANVAS);
        assert_eq!(count(&inv), 200 * 150 - 100 * 100);
        assert_eq!(inv.get(10, 10), 0);
        assert_ne!(inv.get(150, 10), 0);
        assert!(Selection::rect(CANVAS).inverted(CANVAS).is_empty());
    }

    #[test]
    fn grow_and_shrink_by_pixels() {
        let s = Selection::rect(Rect { x0: 50, y0: 50, x1: 60, y1: 60 });
        // 四邻域扩一圈：10×10 的方块变成一个 12×12 去掉四个角。
        assert_eq!(count(&s.grown(1, CANVAS)), 12 * 12 - 4);
        assert_eq!(count(&s.grown(-1, CANVAS)), 8 * 8);
        assert!(s.grown(-5, CANVAS).is_empty());
        // 贴着画布边缘时不会扩到画布外面去。
        let edge = Selection::rect(Rect { x0: 0, y0: 0, x1: 5, y1: 5 }).grown(2, CANVAS);
        assert_eq!(edge.bounds().x0, 0);
        assert_eq!(edge.get(-1, 0), 0);
    }
}
