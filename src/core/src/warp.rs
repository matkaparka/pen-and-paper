// SPDX-License-Identifier: GPL-3.0-or-later
//! 自由变换里不是仿射的两种映射：透视（拖角扭曲）和网格变形（实现方案 §3.8）。
//!
//! 这里只回答"每个目标像素从源图的哪一点取色"，重采样在 `transform::map_tiles` 里。
//! 坐标都是连续的画布坐标：像素 (x, y) 占 [x, x+1)×[y, y+1)，中心在 (x+0.5, y+0.5)。

use std::collections::HashMap;

use crate::tile::{TileCoord, TILE};
use crate::transform::{Affine, Mapping};
use crate::Rect;

/// 3×3 射影矩阵，作用在齐次坐标 (x, y, 1) 上，把源坐标变到目标坐标。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Projective(pub [[f64; 3]; 3]);

/// 什么都放得下的范围：映射说不清结果在哪里时，由调用的地方再和画布取交集。
const EVERYWHERE: Rect = Rect { x0: i32::MIN / 2, y0: i32::MIN / 2, x1: i32::MAX / 2, y1: i32::MAX / 2 };

impl Projective {
    pub const IDENTITY: Projective = Projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    pub fn from_affine(m: &Affine) -> Self {
        let f = |v: f32| v as f64;
        Projective([[f(m.a), f(m.c), f(m.e)], [f(m.b), f(m.d), f(m.f)], [0.0, 0.0, 1.0]])
    }

    /// 单位正方形的四个角 (0,0)、(1,0)、(1,1)、(0,1) 依次映到 `q` 的四个角。
    /// 闭式解见 Heckbert 1989（实现方案 §4）：四边形是平行四边形时退化为仿射变换。
    fn square_to_quad(q: [(f64, f64); 4]) -> Option<Self> {
        let [(x0, y0), (x1, y1), (x2, y2), (x3, y3)] = q;
        let (sx, sy) = (x0 - x1 + x2 - x3, y0 - y1 + y2 - y3);
        let size = (x1 - x0).abs() + (x3 - x0).abs() + (y1 - y0).abs() + (y3 - y0).abs();
        let (g, h) = if sx.abs() + sy.abs() <= size * 1e-12 {
            (0.0, 0.0)
        } else {
            let (dx1, dx2, dy1, dy2) = (x1 - x2, x3 - x2, y1 - y2, y3 - y2);
            let den = dx1 * dy2 - dx2 * dy1;
            if den.abs() <= size * size * 1e-12 {
                return None;
            }
            ((sx * dy2 - dx2 * sy) / den, (dx1 * sy - sx * dy1) / den)
        };
        Some(Projective([[x1 - x0 + g * x1, x3 - x0 + h * x3, x0], [y1 - y0 + g * y1, y3 - y0 + h * y3, y0], [g, h, 1.0]]))
    }

    /// 矩形 `r` 的四个角（左上、右上、右下、左下）依次映到 `q` 的四个角。
    /// `q` 不是凸四边形时没有这样的变换（凹进去或者交叉的四边形里，总有一部分要翻到"地平线"后面去）。
    pub fn rect_to_quad(r: Rect, q: [(f64, f64); 4]) -> Option<Self> {
        if r.is_empty() || !is_convex(&q) {
            return None;
        }
        let (w, h) = ((r.x1 - r.x0) as f64, (r.y1 - r.y0) as f64);
        let to_unit = Projective([[1.0 / w, 0.0, -r.x0 as f64 / w], [0.0, 1.0 / h, -r.y0 as f64 / h], [0.0, 0.0, 1.0]]);
        Some(to_unit.then(&Self::square_to_quad(q)?))
    }

    /// 先做 self，再做 next。
    pub fn then(&self, next: &Projective) -> Projective {
        let (a, b) = (&next.0, &self.0);
        let mut m = [[0.0; 3]; 3];
        for (i, row) in m.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            }
        }
        Projective(m)
    }

    /// 逆变换。奇异或含有非有限值时没有。
    pub fn inverse(&self) -> Option<Projective> {
        let m = &self.0;
        let cof = |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
        let adj = [
            [cof(1, 2, 1, 2), -cof(0, 2, 1, 2), cof(0, 1, 1, 2)],
            [-cof(1, 2, 0, 2), cof(0, 2, 0, 2), -cof(0, 1, 0, 2)],
            [cof(1, 2, 0, 1), -cof(0, 2, 0, 1), cof(0, 1, 0, 1)],
        ];
        let det = m[0][0] * adj[0][0] + m[0][1] * adj[1][0] + m[0][2] * adj[2][0];
        let scale: f64 = m.iter().flatten().map(|v| v.abs()).fold(0.0, f64::max);
        if !det.is_finite() || det.abs() <= scale.powi(3) * 1e-14 {
            return None;
        }
        let inv = Projective(adj.map(|row| row.map(|v| v / det)));
        inv.0.iter().flatten().all(|v| v.is_finite()).then_some(inv)
    }

    /// 映射一点。点在"地平线"上或者后面（齐次坐标的第三个分量不为正）时没有结果。
    pub fn apply(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let m = &self.0;
        let w = m[2][0] * x + m[2][1] * y + m[2][2];
        (w > 1e-12).then(|| ((m[0][0] * x + m[0][1] * y + m[0][2]) / w, (m[1][0] * x + m[1][1] * y + m[1][2]) / w))
    }
}

/// 四边形（顶点顺着一圈排）是不是严格凸的：每个顶点处都朝同一个方向拐。退化成三角形、线段的不算。
pub fn is_convex(q: &[(f64, f64); 4]) -> bool {
    let turn = |i: usize| {
        let (a, b, c) = (q[i], q[(i + 1) % 4], q[(i + 2) % 4]);
        (b.0 - a.0) * (c.1 - b.1) - (b.1 - a.1) * (c.0 - b.0)
    };
    let t = [turn(0), turn(1), turn(2), turn(3)];
    let size = q.iter().map(|p| (p.0 - q[0].0).abs() + (p.1 - q[0].1).abs()).fold(0.0, f64::max);
    let least = size * size * 1e-9;
    t.iter().all(|&v| v > least) || t.iter().all(|&v| v < -least)
}

/// 透视：射影变换的反向映射，逐像素算出源坐标。
pub struct Perspective {
    inv: Projective,
    reach: Rect,
}

impl Perspective {
    /// `forward` 把源坐标变到画布坐标，`from` 是源图上有内容的范围。没有逆变换时没有结果。
    pub fn new(forward: &Projective, from: Rect) -> Option<Perspective> {
        let mut inv = forward.inverse()?;
        // 齐次坐标整体乘一个负数仍是同一个变换；统一成"地平线前面的点第三个分量为正"，
        // 这样反算时一个比较就能挑出没有源点的像素。
        let mid = ((from.x0 as f64 + from.x1 as f64) / 2.0, (from.y0 as f64 + from.y1 as f64) / 2.0);
        let image = forward.apply(mid.0, mid.1)?;
        let m = &inv.0;
        if m[2][0] * image.0 + m[2][1] * image.1 + m[2][2] < 0.0 {
            inv = Projective(inv.0.map(|row| row.map(|v| -v)));
        }
        // 内容四周多留两个像素给插值核。四个角都在地平线前面时，凸四边形的外接矩形就是结果的范围。
        let g = (from.x0 as f64 - 2.0, from.y0 as f64 - 2.0, from.x1 as f64 + 2.0, from.y1 as f64 + 2.0);
        let corners = [(g.0, g.1), (g.2, g.1), (g.2, g.3), (g.0, g.3)].map(|(x, y)| forward.apply(x, y));
        let reach = if corners.iter().all(|c| c.is_some()) { bounding(corners.iter().flatten().copied()) } else { EVERYWHERE };
        Some(Perspective { inv, reach })
    }
}

impl Mapping for Perspective {
    fn reach(&self) -> Rect {
        self.reach
    }

    fn sources(&self, r: Rect, out: &mut [(f64, f64)]) {
        let m = &self.inv.0;
        let w = (r.x1 - r.x0) as usize + 1;
        for (j, row) in out.chunks_mut(w).enumerate() {
            let (x, y) = (r.x0 as f64 + 0.5, r.y0 as f64 + 0.5 + j as f64);
            // 齐次坐标沿一行是线性的，一路加过去。
            let (mut a, mut b, mut c) = (m[0][0] * x + m[0][1] * y + m[0][2], m[1][0] * x + m[1][1] * y + m[1][2], m[2][0] * x + m[2][1] * y + m[2][2]);
            for o in row {
                if c > 1e-12 {
                    *o = (a / c, b / c);
                }
                (a, b, c) = (a + m[0][0], b + m[1][0], c + m[2][0]);
            }
        }
    }
}

/// n×n 个格点上的偏移，按 Catmull-Rom 插值成一张曲面，取参数 (u, v) 处的值。
/// u、v 从 0 到 1 走过整张网格，稍微出界也可以（按两头的那一段外推）。
/// 曲面正好经过每个格点；边上缺的邻点按直线外推，所以边上不会被拉平。
pub fn lattice(offsets: &[(f64, f64)], n: usize, u: f64, v: f64) -> (f64, f64) {
    if n < 2 || offsets.len() < n * n {
        return (0.0, 0.0);
    }
    let seg = (n - 1) as f64;
    let pick = |t: f64| {
        let k = ((t * seg).floor() as isize).clamp(0, n as isize - 2);
        (k, t * seg - k as f64)
    };
    let (ku, tu) = pick(u);
    let (kv, tv) = pick(v);
    let (wu, wv) = (weights(tu), weights(tv));
    // 下标出了网格就按直线外推：p(−1) = 2p(0) − p(1)，另一头同理。
    let ext = |k: isize, get: &dyn Fn(usize) -> (f64, f64)| {
        let last = n - 1;
        if k < 0 {
            let (a, b) = (get(0), get(1));
            (2.0 * a.0 - b.0, 2.0 * a.1 - b.1)
        } else if k as usize > last {
            let (a, b) = (get(last), get(last - 1));
            (2.0 * a.0 - b.0, 2.0 * a.1 - b.1)
        } else {
            get(k as usize)
        }
    };
    let (mut x, mut y) = (0.0, 0.0);
    for (b, &wb) in wv.iter().enumerate() {
        let j = kv - 1 + b as isize;
        for (a, &wa) in wu.iter().enumerate() {
            let i = ku - 1 + a as isize;
            let p = ext(j, &|jj| ext(i, &|ii| offsets[jj * n + ii]));
            x += wa * wb * p.0;
            y += wa * wb * p.1;
        }
    }
    (x, y)
}

/// Catmull-Rom 在相邻四个格点上的权重，t 是到第二个点的距离（0 到 1）。四个权重之和恒为 1。
fn weights(t: f64) -> [f64; 4] {
    let (t2, t3) = (t * t, t * t * t);
    [-0.5 * t3 + t2 - 0.5 * t, 1.5 * t3 - 2.5 * t2 + 1.0, -1.5 * t3 + 2.0 * t2 + 0.5 * t, 0.5 * t3 - 0.5 * t2]
}

/// 网格变形：没有闭式的逆。源区域切成小格，每个格点正向映到画布上，每一格拆成两个三角形，
/// 目标像素落在哪个三角形里，就在三个顶点的源坐标之间按重心坐标插值。
pub struct Mesh {
    /// 切的是源图上的这一块
    from: Rect,
    nx: usize,
    ny: usize,
    /// 格点映到画布上的位置，(nx+1)×(ny+1) 个，逐行；映不过去的是 NaN
    points: Vec<(f64, f64)>,
    /// 每个目标分块要看哪些格子，按格子编号从小到大
    bins: HashMap<TileCoord, Vec<u32>>,
    reach: Rect,
}

impl Mesh {
    /// 目标上大约这么多像素切一格。曲面弯得很缓，16 像素的一段当直线，差不到一个像素。
    const CELL: f64 = 16.0;
    const MAX_CELLS: usize = 1024;

    /// `from` 是要切的源区域（调用的地方应在内容四周多留两个像素给插值核），
    /// `forward` 把源坐标映到画布坐标，映不过去时给 None。
    pub fn new(from: Rect, forward: impl Fn(f64, f64) -> Option<(f64, f64)>) -> Mesh {
        let (x0, y0) = (from.x0 as f64, from.y0 as f64);
        let (w, h) = ((from.x1 - from.x0).max(1) as f64, (from.y1 - from.y0).max(1) as f64);
        let at = |u: f64, v: f64| forward(x0 + u * w, y0 + v * h).unwrap_or((f64::NAN, f64::NAN));
        // 先在 9×9 个点上粗看一下，横竖两个方向映过去最长有多长，按这个决定切多少格。
        let coarse: Vec<(f64, f64)> = (0..81).map(|k| at((k % 9) as f64 / 8.0, (k / 9) as f64 / 8.0)).collect();
        let along = |across: bool| {
            (0..9)
                .map(|a| {
                    (0..8)
                        .map(|b| {
                            let (p, q) = if across { (coarse[a * 9 + b], coarse[a * 9 + b + 1]) } else { (coarse[b * 9 + a], coarse[(b + 1) * 9 + a]) };
                            let d = (q.0 - p.0).hypot(q.1 - p.1);
                            if d.is_finite() {
                                d
                            } else {
                                0.0
                            }
                        })
                        .sum::<f64>()
                })
                .fold(0.0, f64::max)
        };
        let count = |len: f64, side: f64| ((len.max(side) / Self::CELL).ceil() as usize).clamp(2, Self::MAX_CELLS);
        let (nx, ny) = (count(along(true), w), count(along(false), h));
        let points: Vec<(f64, f64)> = (0..=ny).flat_map(|j| (0..=nx).map(move |i| (i, j))).map(|(i, j)| at(i as f64 / nx as f64, j as f64 / ny as f64)).collect();

        let mut bins: HashMap<TileCoord, Vec<u32>> = HashMap::new();
        let mut reach = Rect::EMPTY;
        for j in 0..ny {
            for i in 0..nx {
                let k = j * (nx + 1) + i;
                let quad = [points[k], points[k + 1], points[k + nx + 2], points[k + nx + 1]];
                if quad.iter().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
                    continue;
                }
                let b = bounding(quad.iter().copied());
                reach = reach.union(b);
                // 每个分块还要多算右边一列、下边一行（见 `Mapping::sources`），所以格子往左、往上多算一个像素。
                let b = Rect { x0: b.x0 - 1, y0: b.y0 - 1, ..b };
                let t = TILE;
                for ty in b.y0.div_euclid(t)..=(b.y1 - 1).div_euclid(t) {
                    for tx in b.x0.div_euclid(t)..=(b.x1 - 1).div_euclid(t) {
                        bins.entry((tx, ty)).or_default().push((j * nx + i) as u32);
                    }
                }
            }
        }
        Mesh { from, nx, ny, points, bins, reach }
    }

    /// 格点 (i, j) 在源图上的位置。
    fn source(&self, i: usize, j: usize) -> (f64, f64) {
        let (w, h) = ((self.from.x1 - self.from.x0).max(1) as f64, (self.from.y1 - self.from.y0).max(1) as f64);
        (self.from.x0 as f64 + w * i as f64 / self.nx as f64, self.from.y0 as f64 + h * j as f64 / self.ny as f64)
    }
}

impl Mapping for Mesh {
    fn reach(&self) -> Rect {
        self.reach
    }

    fn sources(&self, r: Rect, out: &mut [(f64, f64)]) {
        let Some(cells) = self.bins.get(&(r.x0.div_euclid(TILE), r.y0.div_euclid(TILE))) else { return };
        let area = Rect { x1: r.x1 + 1, y1: r.y1 + 1, ..r };
        for &cell in cells {
            let (i, j) = (cell as usize % self.nx, cell as usize / self.nx);
            let k = j * (self.nx + 1) + i;
            let p = [self.points[k], self.points[k + 1], self.points[k + self.nx + 2], self.points[k + self.nx + 1]];
            let s = [self.source(i, j), self.source(i + 1, j), self.source(i + 1, j + 1), self.source(i, j + 1)];
            triangle(area, out, [p[0], p[1], p[2]], [s[0], s[1], s[2]]);
            triangle(area, out, [p[0], p[2], p[3]], [s[0], s[2], s[3]]);
        }
    }
}

/// 把一个三角形光栅化进 `out`（覆盖矩形 `area`，逐行）：中心落在三角形里的像素，
/// 源坐标按重心坐标在三个顶点的源坐标 `s` 之间插值。边上的像素两边的三角形都算，宁可重复，不留缝。
fn triangle(area: Rect, out: &mut [(f64, f64)], p: [(f64, f64); 3], s: [(f64, f64); 3]) {
    let (e1, e2) = ((p[1].0 - p[0].0, p[1].1 - p[0].1), (p[2].0 - p[0].0, p[2].1 - p[0].1));
    let d = e1.0 * e2.1 - e1.1 * e2.0;
    if d.abs() < 1e-12 {
        return;
    }
    let lo = |a: f64, b: f64, c: f64| a.min(b).min(c);
    let hi = |a: f64, b: f64, c: f64| a.max(b).max(c);
    // 中心 x + 0.5 落在 [最小, 最大] 之间的像素。
    let x0 = ((lo(p[0].0, p[1].0, p[2].0) - 0.5).ceil() as i64).max(area.x0 as i64);
    let x1 = ((hi(p[0].0, p[1].0, p[2].0) - 0.5).floor() as i64).min(area.x1 as i64 - 1);
    let y0 = ((lo(p[0].1, p[1].1, p[2].1) - 0.5).ceil() as i64).max(area.y0 as i64);
    let y1 = ((hi(p[0].1, p[1].1, p[2].1) - 0.5).floor() as i64).min(area.y1 as i64 - 1);
    let w = (area.x1 - area.x0) as i64;
    const EPS: f64 = 1e-9;
    for y in y0..=y1 {
        let cy = y as f64 + 0.5 - p[0].1;
        for x in x0..=x1 {
            let cx = x as f64 + 0.5 - p[0].0;
            let b1 = (cx * e2.1 - cy * e2.0) / d;
            let b2 = (e1.0 * cy - e1.1 * cx) / d;
            if b1 >= -EPS && b2 >= -EPS && b1 + b2 <= 1.0 + EPS {
                out[((y - area.y0 as i64) * w + (x - area.x0 as i64)) as usize] =
                    (s[0].0 + b1 * (s[1].0 - s[0].0) + b2 * (s[2].0 - s[0].0), s[0].1 + b1 * (s[1].1 - s[0].1) + b2 * (s[2].1 - s[0].1));
            }
        }
    }
}

/// 一组点的外接矩形，向外取整再多留一个像素。
fn bounding(points: impl Iterator<Item = (f64, f64)>) -> Rect {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (x, y) in points {
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
    }
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return Rect::EMPTY;
    }
    let fit = |v: f64| v.clamp(EVERYWHERE.x0 as f64, EVERYWHERE.x1 as f64) as i32;
    Rect { x0: fit(x0.floor() - 1.0), y0: fit(y0.floor() - 1.0), x1: fit(x1.ceil() + 1.0), y1: fit(y1.ceil() + 1.0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Selection;
    use crate::tile::{tiles_in, TileMap};
    use crate::transform::{map_selection, map_tiles, transform_tiles, Quality};
    use crate::ONE;
    use std::time::Instant;

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    fn at(tiles: &TileMap, x: i32, y: i32) -> [u16; 4] {
        let i = (y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize * 4;
        tiles.get((x.div_euclid(TILE), y.div_euclid(TILE))).map_or([0; 4], |t| [t.0[i], t.0[i + 1], t.0[i + 2], t.0[i + 3]])
    }

    fn paint(tiles: &mut TileMap, r: Rect, f: impl Fn(i32, i32) -> [u16; 4]) {
        for tc in tiles_in(r) {
            let t = tiles.get_mut(tc);
            let (ox, oy) = (tc.0 * TILE, tc.1 * TILE);
            for y in r.y0.max(oy)..r.y1.min(oy + TILE) {
                for x in r.x0.max(ox)..r.x1.min(ox + TILE) {
                    let i = ((y - oy) * TILE + (x - ox)) as usize * 4;
                    t.0[i..i + 4].copy_from_slice(&f(x, y));
                }
            }
        }
    }

    /// 伪随机的预乘像素，与 `transform` 的测试用的同一种。
    fn noise(x: i32, y: i32) -> [u16; 4] {
        let mut h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA6B);
        h ^= h >> 15;
        h = h.wrapping_mul(0xC2B2_AE35);
        h ^= h >> 13;
        let a = match h % 5 {
            0 => ONE,
            1 => 0,
            _ => (h >> 8) % (ONE + 1),
        };
        let c = |k: u32| ((h >> k) % (a + 1)) as u16;
        [c(3), c(7), c(11), a as u16]
    }

    fn noise_in(r: Rect) -> TileMap {
        let mut t = TileMap::default();
        paint(&mut t, r, noise);
        t
    }

    fn pixels_in(r: Rect) -> impl Iterator<Item = (i32, i32)> {
        (r.y0..r.y1).flat_map(move |y| (r.x0..r.x1).map(move |x| (x, y)))
    }

    /// 结果必须是合法的预乘像素，画布外面什么都没有。
    fn assert_tidy(out: &TileMap, clip: Rect) {
        for tc in out.coords() {
            for (i, p) in out.get(tc).unwrap().0.as_chunks::<4>().0.iter().enumerate() {
                let (x, y) = (tc.0 * TILE + i as i32 % TILE, tc.1 * TILE + i as i32 / TILE);
                let inside = x >= clip.x0 && x < clip.x1 && y >= clip.y0 && y < clip.y1;
                assert!(inside || *p == [0; 4], "({x}, {y}) 在 clip 之外却有内容");
                assert!(p[3] as u32 <= ONE && p[..3].iter().all(|&c| c <= p[3]), "({x}, {y}) 不是合法的预乘像素：{p:?}");
            }
        }
    }

    fn assert_close(a: &TileMap, b: &TileMap, area: Rect, tolerance: u16, what: &str) {
        for (x, y) in pixels_in(area) {
            let (p, q) = (at(a, x, y), at(b, x, y));
            assert!(p.iter().zip(q).all(|(&u, v)| u.abs_diff(v) <= tolerance), "{what} ({x}, {y}): {p:?} 对 {q:?}");
        }
    }

    fn close(p: (f64, f64), q: (f64, f64)) -> bool {
        (p.0 - q.0).abs() < 1e-6 && (p.1 - q.1).abs() < 1e-6
    }

    #[test]
    fn a_rectangle_lands_exactly_on_its_quad() {
        let r = rect(40, 30, 140, 110);
        let quads = [
            [(50.0, 20.0), (170.0, 45.0), (150.0, 130.0), (30.0, 100.0)],
            // 只是一个平行四边形：退化成仿射变换。
            [(40.0, 30.0), (140.0, 50.0), (140.0, 130.0), (40.0, 110.0)],
            // 反着绕的也是凸的（翻了面）。
            [(140.0, 30.0), (40.0, 30.0), (40.0, 110.0), (140.0, 110.0)],
        ];
        for q in quads {
            let m = Projective::rect_to_quad(r, q).unwrap();
            for (c, want) in [(40.0, 30.0), (140.0, 30.0), (140.0, 110.0), (40.0, 110.0)].into_iter().zip(q) {
                assert!(close(m.apply(c.0, c.1).unwrap(), want), "{q:?} {c:?}");
            }
            let inv = m.inverse().unwrap();
            for p in [(55.5, 41.25), (90.0, 70.0), (139.0, 31.0)] {
                let there = m.apply(p.0, p.1).unwrap();
                assert!(close(inv.apply(there.0, there.1).unwrap(), p), "{q:?} {p:?}");
            }
        }
        let flat = Projective::rect_to_quad(r, quads[1]).unwrap();
        assert!(flat.0[2][0].abs() < 1e-12 && flat.0[2][1].abs() < 1e-12);
        // 先后两个变换拼起来，和分别做一遍一样。
        let a = Projective::rect_to_quad(r, quads[0]).unwrap();
        let b = Projective::from_affine(&Affine::rotate_about(0.3, 90.0, 70.0));
        let mid = a.apply(70.0, 50.0).unwrap();
        assert!(close(a.then(&b).apply(70.0, 50.0).unwrap(), b.apply(mid.0, mid.1).unwrap()));
    }

    #[test]
    fn concave_and_crossed_quads_are_refused() {
        let r = rect(0, 0, 100, 100);
        let square = [(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0)];
        assert!(is_convex(&square));
        // 一个角拖进了对角线里面：凹的。
        assert!(!is_convex(&[(0.0, 0.0), (100.0, 0.0), (30.0, 30.0), (0.0, 100.0)]));
        // 两个角对调：交叉的"蝴蝶结"。
        assert!(!is_convex(&[(0.0, 0.0), (100.0, 0.0), (0.0, 100.0), (100.0, 100.0)]));
        // 三个点在一条线上、所有点重合：退化的。
        assert!(!is_convex(&[(0.0, 0.0), (50.0, 0.0), (100.0, 0.0), (0.0, 100.0)]));
        assert!(!is_convex(&[(5.0, 5.0); 4]));
        assert!(Projective::rect_to_quad(r, [(0.0, 0.0), (100.0, 0.0), (30.0, 30.0), (0.0, 100.0)]).is_none());
        assert!(Projective::rect_to_quad(Rect::EMPTY, square).is_none());
        assert!(Projective([[1.0, 2.0, 0.0], [2.0, 4.0, 0.0], [0.0, 0.0, 1.0]]).inverse().is_none());
        assert!(Projective([[f64::NAN, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]).inverse().is_none());
    }

    #[test]
    fn perspective_of_an_affine_matches_the_affine_path() {
        let clip = rect(0, 0, 220, 180);
        let area = rect(40, 35, 150, 120);
        let src = noise_in(area);
        let cases = [
            Affine::rotate_about(0.4, 100.0, 80.0).then(&Affine::scale_about(1.3, 0.8, 100.0, 80.0)).then(&Affine::translate(3.25, -2.5)),
            Affine::scale_about(0.45, 0.6, 90.0, 70.0),
        ];
        for m in cases {
            let p = Perspective::new(&Projective::from_affine(&m), area).unwrap();
            for quality in [Quality::Fast, Quality::Best] {
                let want = transform_tiles(&src, None, &m, quality, clip);
                let got = map_tiles(&src, None, &p, quality, clip);
                assert_tidy(&got, clip);
                // 两条路算源坐标的方式不同（逐行累加定点数、逐像素浮点），差一两个单位。
                assert_close(&got, &want, clip, 3, &format!("{m:?} {quality:?}"));
            }
        }
    }

    #[test]
    fn perspective_fills_its_quad_and_nothing_else() {
        let clip = rect(0, 0, 260, 200);
        let r = rect(60, 50, 160, 130);
        let mut src = TileMap::default();
        paint(&mut src, r, |_, _| [ONE as u16; 4]);
        // 下边宽、上边窄的梯形，像一块往后倒的板子。
        let q = [(90.0, 40.0), (150.0, 40.0), (220.0, 170.0), (20.0, 170.0)];
        let p = Perspective::new(&Projective::rect_to_quad(r, q).unwrap(), r).unwrap();
        for quality in [Quality::Fast, Quality::Best] {
            let out = map_tiles(&src, None, &p, quality, clip);
            assert_tidy(&out, clip);
            // 四条边往里几个像素是实的，往外两个像素是空的。下边放大了五倍左右，往里要多留一点，
            // 不然取到的还是源图边上那半个像素的过渡。
            for (inside, outside) in [((120, 43), (120, 37)), ((40, 162), (20, 160)), ((200, 162), (230, 167)), ((120, 162), (120, 173))] {
                assert_eq!(at(&out, inside.0, inside.1)[3] as u32, ONE, "{quality:?} {inside:?}");
                assert_eq!(at(&out, outside.0, outside.1)[3], 0, "{quality:?} {outside:?}");
            }
            // 面积是梯形的面积。
            let area: f64 = pixels_in(clip).map(|(x, y)| at(&out, x, y)[3] as f64 / ONE as f64).sum();
            let want = (60.0 + 200.0) / 2.0 * 130.0;
            assert!((area / want - 1.0).abs() < 0.01, "{quality:?}: {area} 对 {want}");
        }
        // 选区跟着走：内容落到哪里，选区就在哪里。
        let sel = Selection::rect(r);
        let moved = map_selection(&sel, &p, clip);
        assert_eq!(moved.get(120, 100) as u32, ONE);
        assert_eq!(moved.get(30, 60), 0);
        assert!(map_selection(&Selection::empty(), &p, clip).is_empty());
    }

    #[test]
    fn identity_mesh_copies_exactly() {
        let clip = rect(0, 0, 200, 150);
        let area = rect(30, 20, 170, 130);
        let src = noise_in(area);
        let mesh = Mesh::new(rect(28, 18, 172, 132), |x, y| Some((x, y)));
        for quality in [Quality::Fast, Quality::Best] {
            let out = map_tiles(&src, None, &mesh, quality, clip);
            assert_close(&out, &src, clip, 0, &format!("{quality:?}"));
        }
    }

    #[test]
    fn mesh_of_an_affine_matches_the_affine_path() {
        let clip = rect(0, 0, 240, 200);
        let area = rect(50, 40, 160, 140);
        let src = noise_in(area);
        let m = Affine::rotate_about(-0.35, 105.0, 90.0).then(&Affine::scale_about(1.4, 1.1, 105.0, 90.0));
        let mesh = Mesh::new(rect(48, 38, 162, 142), |x, y| {
            let p = m.apply(x as f32, y as f32);
            Some((p.0 as f64, p.1 as f64))
        });
        for quality in [Quality::Fast, Quality::Best] {
            let want = transform_tiles(&src, None, &m, quality, clip);
            let got = map_tiles(&src, None, &mesh, quality, clip);
            assert_tidy(&got, clip);
            // 正向映射用的是 f32 的仿射矩阵，三角形内又是线性插值，比上面多差一点。
            assert_close(&got, &want, clip, 6, &format!("{quality:?}"));
        }
    }

    #[test]
    fn the_lattice_passes_through_its_points() {
        let n = 4;
        let offsets: Vec<(f64, f64)> = (0..n * n).map(|k| (((k * 37) % 11) as f64 - 5.0, ((k * 53) % 7) as f64 - 3.0)).collect();
        for j in 0..n {
            for i in 0..n {
                let (u, v) = (i as f64 / 3.0, j as f64 / 3.0);
                assert!(close(lattice(&offsets, n, u, v), offsets[j * n + i]), "({i}, {j})");
            }
        }
        // 全零就处处为零；格点排成一个仿射的样子，曲面也就是那个仿射函数（直线外推保住了这一点）。
        assert_eq!(lattice(&[(0.0, 0.0); 16], 4, 0.37, 0.81), (0.0, 0.0));
        let linear: Vec<(f64, f64)> = (0..16).map(|k| ((k % 4) as f64 * 2.0 + (k / 4) as f64, (k / 4) as f64 * -3.0)).collect();
        for (u, v) in [(0.1, 0.2), (0.5, 0.5), (0.93, 0.07), (-0.02, 1.01)] {
            let want = (u * 3.0 * 2.0 + v * 3.0, v * 3.0 * -3.0);
            assert!(close(lattice(&linear, 4, u, v), want), "({u}, {v})");
        }
        // 一个点挪开，曲面在它旁边平滑地跟过去，离得越远影响越小。
        let mut one = [(0.0, 0.0); 16];
        one[5] = (10.0, 0.0);
        let near = lattice(&one, 4, 1.0 / 3.0 + 0.05, 1.0 / 3.0).0;
        let far = lattice(&one, 4, 0.95, 0.95).0;
        assert!(near > 8.0 && near < 10.0 && far.abs() < 1.0, "{near} {far}");
        assert_eq!(lattice(&one, 1, 0.5, 0.5), (0.0, 0.0));
    }

    #[test]
    fn bending_the_mesh_moves_what_is_under_the_point() {
        let clip = rect(0, 0, 300, 300);
        let r = rect(60, 60, 240, 240);
        let mut src = TileMap::default();
        // 一条两像素宽的竖线，正好穿过网格左起第二列的格点（x = 60 + 180/3 = 120）。
        paint(&mut src, rect(119, 60, 121, 240), |_, _| [0, 0, 0, ONE as u16]);
        let mut offsets = [(0.0, 0.0); 16];
        offsets[5] = (20.0, 0.0);
        let (w, h) = ((r.x1 - r.x0) as f64, (r.y1 - r.y0) as f64);
        let forward = |x: f64, y: f64| {
            let o = lattice(&offsets, 4, (x - r.x0 as f64) / w, (y - r.y0 as f64) / h);
            Some((x + o.0, y + o.1))
        };
        let mesh = Mesh::new(rect(58, 58, 242, 242), forward);
        let out = map_tiles(&src, None, &mesh, Quality::Best, clip);
        assert_tidy(&out, clip);
        // 格点 (1, 1) 在 (120, 120)，挪到了 (140, 120)：那里现在是线，原来的位置空了。
        assert!(at(&out, 140, 120)[3] as u32 > ONE / 2, "{:?}", at(&out, 140, 120));
        assert_eq!(at(&out, 120, 120)[3], 0);
        // 线的两头没动。
        assert!(at(&out, 120, 62)[3] as u32 > ONE / 2 && at(&out, 120, 237)[3] as u32 > ONE / 2);
        // 线没有断：每一行都还有墨。
        for y in 62..238 {
            let ink: u32 = (60..240).map(|x| at(&out, x, y)[3] as u32).sum();
            assert!(ink > ONE, "第 {y} 行断了");
        }
    }

    #[test]
    fn degenerate_maps_do_not_panic() {
        let clip = rect(0, 0, 120, 100);
        let src = noise_in(rect(10, 10, 90, 80));
        assert!(Perspective::new(&Projective([[0.0; 3]; 3]), rect(10, 10, 90, 80)).is_none());
        // 映不过去的网格：什么都不出。
        let none = Mesh::new(rect(8, 8, 92, 82), |_, _| None);
        assert!(map_tiles(&src, None, &none, Quality::Best, clip).is_empty());
        let nan = Mesh::new(rect(8, 8, 92, 82), |_, _| Some((f64::NAN, 1.0)));
        assert!(map_tiles(&src, None, &nan, Quality::Fast, clip).is_empty());
        // 把整块压成一个点、拉得极大、甩到极远、上下翻过来：结果是什么不要紧，不能崩，也不能越界。
        type Forward = fn(f64, f64) -> Option<(f64, f64)>;
        let odd: [Forward; 4] = [|_, _| Some((50.0, 50.0)), |x, y| Some((x * 1e6, y)), |x, y| Some((x + 1e12, y)), |x, y| Some((x, 100.0 - y))];
        for f in odd {
            let mesh = Mesh::new(rect(8, 8, 92, 82), f);
            for quality in [Quality::Fast, Quality::Best] {
                assert_tidy(&map_tiles(&src, None, &mesh, quality, clip), clip);
            }
        }
        let flat = Mesh::new(Rect::EMPTY, |x, y| Some((x, y)));
        assert_tidy(&map_tiles(&src, None, &flat, Quality::Fast, clip), clip);
    }

    /// 铺满的大图层上透视和网格各要多久，手动跑着看：
    /// `cargo test -p zhibi-core --release warp::tests::timings -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn timings() {
        let clip = rect(0, 0, 3000, 2000);
        let mut src = TileMap::default();
        paint(&mut src, clip, |x, y| {
            let a = (ONE / 4 + ((x * 7 + y * 13) as u32 % (ONE * 3 / 4))) as u16;
            [a / 2, (x as u32 * 16 % a as u32) as u16, a, a]
        });
        let q = [(150.0, 100.0), (2900.0, 0.0), (2700.0, 1900.0), (0.0, 2000.0)];
        let perspective = Perspective::new(&Projective::rect_to_quad(clip, q).unwrap(), clip).unwrap();
        let mut offsets = [(0.0, 0.0); 16];
        offsets[5] = (120.0, -80.0);
        offsets[10] = (-60.0, 150.0);
        let bend = |x: f64, y: f64| {
            let o = lattice(&offsets, 4, x / 3000.0, y / 2000.0);
            Some((x + o.0, y + o.1))
        };
        let started = Instant::now();
        let mesh = Mesh::new(rect(-2, -2, 3002, 2002), bend);
        let build_ms = started.elapsed().as_secs_f64() * 1000.0;
        let profile = if cfg!(debug_assertions) { "dev" } else { "release" };
        for (name, m) in [("透视", &perspective as &dyn Mapping), ("网格", &mesh as &dyn Mapping)] {
            for quality in [Quality::Fast, Quality::Best] {
                let best = (0..3)
                    .map(|_| {
                        let t = Instant::now();
                        let out = map_tiles(&src, None, m, quality, clip);
                        assert!(!out.is_empty());
                        t.elapsed().as_secs_f64() * 1000.0
                    })
                    .fold(f64::MAX, f64::min);
                eprintln!("[{profile}] {name} {quality:?}：{best:.1} ms");
            }
        }
        eprintln!("[{profile}] 建网格：{build_ms:.1} ms");
    }
}
