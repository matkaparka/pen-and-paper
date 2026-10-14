// SPDX-License-Identifier: GPL-3.0-or-later
//! 笔画引擎：稳定器 → 样本之间用曲线连接 → 沿曲线画"变粗细的胶囊" → 记进这一笔的覆盖率层。
//!
//! 一笔之内每个像素的覆盖率取最大值而不是累加，所以来回涂不会越涂越深；
//! 抬笔时由文档把整笔按浓度并入图层（规格的实现方案 §3.2、§3.3）。

use crate::stabilizer::{Sample, Stabilizer};
use crate::tile::{tiles_in, MaskMap, TileCoord, TILE};
use crate::{Rect, ONE};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrushParams {
    /// 满压时的直径，画布像素
    pub size: f32,
    /// 最轻压力下的直径占满压直径的比例，0 到 1
    pub min_size: f32,
    /// 浓度，0 到 1
    pub density: f32,
    /// 最轻压力下的浓度占设定浓度的比例，0 到 1；为 1 时压力不影响浓度
    pub min_density: f32,
    /// 边缘硬度：1 为硬边（只有一个像素的抗锯齿过渡），越小边缘越软
    pub hardness: f32,
    pub color: [u8; 3],
    /// 橡皮：并入图层时减少不透明度而不是上色
    pub erase: bool,
    /// 马克笔（规格 F-031）：不透明度只升到这一笔的浓度为止，叠涂不会越叠越浓；颜色向这一笔的颜色过渡
    pub marker: bool,
}

impl Default for BrushParams {
    fn default() -> Self {
        Self { size: 10.0, min_size: 0.0, density: 1.0, min_density: 1.0, hardness: 1.0, color: [0, 0, 0], erase: false, marker: false }
    }
}

impl BrushParams {
    /// 压力到半径：线性（规格 F-011、F-013）。
    pub fn radius(&self, pressure: f32) -> f32 {
        0.5 * self.size * (self.min_size + (1.0 - self.min_size) * pressure.clamp(0.0, 1.0))
    }

    /// 压力到浓度的比例：线性（规格 F-012）。
    fn flow(&self, pressure: f32) -> f32 {
        self.min_density + (1.0 - self.min_density) * pressure.clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy)]
struct Node {
    x: f32,
    y: f32,
    /// 半径
    r: f32,
    /// 这一点的浓度比例，0 到 1
    f: f32,
}

pub struct Stroke {
    pub params: BrushParams,
    /// 已经定下来的覆盖率
    pub mask: MaskMap,
    /// 最新一段的暂定覆盖率：先按直线画到笔尖，下一个样本到来后改成曲线并入 `mask`。
    /// 这样墨迹不用等下一个样本就能跟到笔尖。
    tail: MaskMap,
    tail_bounds: Rect,
    stab: Stabilizer,
    /// 最近的稳定器输出点，最多保留三个
    nodes: Vec<Node>,
}

impl Stroke {
    /// 落笔。返回被改动的画布区域。
    pub fn begin(params: BrushParams, stabilizer_level: usize, s: Sample) -> (Self, Rect) {
        let mut stab = Stabilizer::new(stabilizer_level);
        let o = stab.begin(s);
        let node = Node { x: o.x, y: o.y, r: params.radius(o.p), f: params.flow(o.p) };
        let mut stroke = Self {
            params,
            mask: MaskMap::default(),
            tail: MaskMap::default(),
            tail_bounds: Rect::EMPTY,
            stab,
            nodes: vec![node],
        };
        // 落笔即出墨：原地点一下也要留下一个点（规格 F-020）。
        let dirty = capsule(&mut stroke.mask, None, params.hardness, node, node);
        (stroke, dirty)
    }

    pub fn add(&mut self, s: Sample) -> Rect {
        let o = self.stab.push(s);
        self.extend_to(o)
    }

    /// 抬笔：把稳定器里还没画出来的部分补完（规格 F-017），并把暂定的最后一段定下来。
    pub fn end(&mut self) -> Rect {
        let mut dirty = Rect::EMPTY;
        for o in self.stab.finish() {
            dirty = dirty.union(self.extend_to(o));
        }
        let n = self.nodes.len();
        if n >= 2 {
            let before = if n >= 3 { self.nodes[n - 3] } else { self.nodes[n - 2] };
            dirty = dirty.union(self.curve(before, self.nodes[n - 2], self.nodes[n - 1], self.nodes[n - 1]));
        }
        dirty = dirty.union(self.tail_bounds);
        self.tail = MaskMap::default();
        self.tail_bounds = Rect::EMPTY;
        dirty
    }

    /// 某个分块上的覆盖率：已定下来的部分和暂定的尾段，显示时取两者较大的。
    pub fn masks(&self, tc: TileCoord) -> (Option<&[u16]>, Option<&[u16]>) {
        (self.mask.get(tc), self.tail.get(tc))
    }

    fn extend_to(&mut self, o: Sample) -> Rect {
        let node = Node { x: o.x, y: o.y, r: self.params.radius(o.p), f: self.params.flow(o.p) };
        let mut dirty = self.tail_bounds;
        let n = self.nodes.len();
        // 新点到来后，上一段的走向确定了，把它按曲线定下来。
        if n >= 2 {
            let before = if n >= 3 { self.nodes[n - 3] } else { self.nodes[n - 2] };
            dirty = dirty.union(self.curve(before, self.nodes[n - 2], self.nodes[n - 1], node));
        }
        // 最新一段先按直线暂定。已定下来的部分在哪里已经够浓了，那里的尾段就不必算：显示时取两者较大的。
        self.tail = MaskMap::default();
        self.tail_bounds = capsule(&mut self.tail, Some(&self.mask), self.params.hardness, self.nodes[n - 1], node);
        dirty = dirty.union(self.tail_bounds);
        self.nodes.push(node);
        if self.nodes.len() > 3 {
            self.nodes.remove(0);
        }
        dirty
    }

    /// 把 p1 到 p2 这一段按向心 Catmull-Rom 曲线画进 `mask`。p0、p3 是两侧的相邻点。
    ///
    /// 向心参数化不会在急转弯和密集样本处产生尖刺或自交
    ///（Yuksel、Schaefer、Keyser，2011，见实现方案 §4）。
    fn curve(&mut self, p0: Node, p1: Node, p2: Node, p3: Node) -> Rect {
        let len = ((p2.x - p1.x).powi(2) + (p2.y - p1.y).powi(2)).sqrt();
        // 每一小段都是一个完整的胶囊，大笔刷时一个胶囊就是一大片像素。曲线分段是为了让中线跟着曲线走，
        // 对粗笔来说分得比半径的一半还细，边缘位置的差别不到一个像素，只是白白把同一片地方重算很多遍。
        let piece = (0.5 * p1.r.min(p2.r)).max(2.5);
        let pieces = ((len / piece).ceil() as usize).clamp(1, 48);
        if pieces == 1 {
            return capsule(&mut self.mask, None, self.params.hardness, p1, p2);
        }
        // 两端缺邻点（起笔、收笔）或邻点重合时，用镜像点代替，曲线在那一端退化为直线走向。
        let fix = |q: Node, near: Node, far: Node| {
            if (q.x - near.x).abs() + (q.y - near.y).abs() < 1e-3 {
                (2.0 * near.x - far.x, 2.0 * near.y - far.y)
            } else {
                (q.x, q.y)
            }
        };
        let a = fix(p0, p1, p2);
        let d = fix(p3, p2, p1);
        let (b, c) = ((p1.x, p1.y), (p2.x, p2.y));
        let knot = |p: (f32, f32), q: (f32, f32)| ((q.0 - p.0).powi(2) + (q.1 - p.1).powi(2)).sqrt().sqrt().max(1e-3);
        let t1 = knot(a, b);
        let t2 = t1 + knot(b, c);
        let t3 = t2 + knot(c, d);
        let lerp = |p: (f32, f32), q: (f32, f32), ta: f32, tb: f32, t: f32| {
            let u = (t - ta) / (tb - ta);
            (p.0 + (q.0 - p.0) * u, p.1 + (q.1 - p.1) * u)
        };
        let mut dirty = Rect::EMPTY;
        let mut prev = p1;
        for j in 1..=pieces {
            let u = j as f32 / pieces as f32;
            let t = t1 + (t2 - t1) * u;
            let a1 = lerp(a, b, 0.0, t1, t);
            let a2 = lerp(b, c, t1, t2, t);
            let a3 = lerp(c, d, t2, t3, t);
            let b1 = lerp(a1, a2, 0.0, t2, t);
            let b2 = lerp(a2, a3, t1, t3, t);
            let (x, y) = lerp(b1, b2, t1, t2, t);
            let node = Node { x, y, r: p1.r + (p2.r - p1.r) * u, f: p1.f + (p2.f - p1.f) * u };
            dirty = dirty.union(capsule(&mut self.mask, None, self.params.hardness, prev, node));
            prev = node;
        }
        dirty
    }
}

/// 胶囊碰到的像素不少于这么多时，按分块分给几个线程去算。小笔刷一段只有几百个像素，不值得开线程。
const PARALLEL_PIXELS: i64 = 60_000;

/// 从 a 到 b 画一段粗细和浓度线性变化的胶囊，覆盖率与已有值取最大。
///
/// 已经达到这一段最大浓度的像素不用再算：画笔往前挪一小步时，新胶囊的大半落在已经涂满的地方。
/// `under` 是另一张覆盖率表（画暂定的尾段时传入已定下来的部分），那里够浓的像素也一样跳过。
fn capsule(mask: &mut MaskMap, under: Option<&MaskMap>, hardness: f32, a: Node, b: Node) -> Rect {
    let rmax = a.r.max(b.r);
    let bounds = Rect {
        x0: (a.x.min(b.x) - rmax - 1.0).floor() as i32,
        y0: (a.y.min(b.y) - rmax - 1.0).floor() as i32,
        x1: (a.x.max(b.x) + rmax + 1.0).ceil() as i32 + 1,
        y1: (a.y.max(b.y) + rmax + 1.0).ceil() as i32 + 1,
    };
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    let hardness = hardness.clamp(0.0, 1.0);
    let cap = (a.f.max(b.f).clamp(0.0, 1.0) * ONE as f32 + 0.5) as u16;
    // 覆盖率大于 0 的范围离中线不超过 r + 0.5；多留半个像素。
    let reach = rmax + 1.0;

    let paint = |(tx, ty): TileCoord, tile: &mut [u16]| {
        let below = under.and_then(|m| m.get((tx, ty)));
        let (ox, oy) = (tx * TILE, ty * TILE);
        for py in bounds.y0.max(oy)..bounds.y1.min(oy + TILE) {
            let cy = py as f32 + 0.5;
            // 只走这一行里落在胶囊范围内的那一段像素。
            let Some((lo, hi)) = row_span(a, b, reach, cy) else { continue };
            let xa = ((lo - 0.5).floor() as i32).max(bounds.x0).max(ox);
            let xb = ((hi - 0.5).ceil() as i32 + 1).min(bounds.x1).min(ox + TILE);
            for px in xa..xb {
                let i = ((py - oy) * TILE + (px - ox)) as usize;
                if tile[i] >= cap || below.is_some_and(|m| m[i] >= cap) {
                    continue;
                }
                let cx = px as f32 + 0.5;
                // 像素中心在线段上的投影位置 t，以及该处的半径和浓度比例。
                let t = if len2 > 0.0 { (((cx - a.x) * dx + (cy - a.y) * dy) / len2).clamp(0.0, 1.0) } else { 1.0 };
                let (qx, qy) = (a.x + dx * t - cx, a.y + dy * t - cy);
                let r = a.r + (b.r - a.r) * t;
                let c = coverage(r, (qx * qx + qy * qy).sqrt(), hardness) * (a.f + (b.f - a.f) * t);
                if c > 0.0 {
                    let v = (c * ONE as f32 + 0.5) as u16;
                    if v > tile[i] {
                        tile[i] = v;
                    }
                }
            }
        }
    };

    let coords: Vec<TileCoord> = tiles_in(bounds).collect();
    let area = (bounds.x1 - bounds.x0) as i64 * (bounds.y1 - bounds.y0) as i64;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let mut tiles = mask.many_mut(&coords);
    if area >= PARALLEL_PIXELS && threads > 1 && tiles.len() > 1 {
        let per = tiles.len().div_ceil(threads);
        let paint = &paint;
        std::thread::scope(|scope| {
            for part in tiles.chunks_mut(per) {
                scope.spawn(move || {
                    for (tc, tile) in part.iter_mut() {
                        paint(*tc, tile);
                    }
                });
            }
        });
    } else {
        for (tc, tile) in tiles.iter_mut() {
            paint(*tc, tile);
        }
    }
    bounds
}

/// 水平线 y = cy 与"离线段 ab 不超过 reach"这个区域相交的那一段 x 范围。
/// 这个区域是两头两个圆加中间一条带，是凸的，所以交出来是一段；分别求三块再取并集的两端即可。
fn row_span(a: Node, b: Node, reach: f32, cy: f32) -> Option<(f32, f32)> {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for p in [a, b] {
        let h = cy - p.y;
        if h.abs() <= reach {
            let w = (reach * reach - h * h).sqrt();
            (lo, hi) = (lo.min(p.x - w), hi.max(p.x + w));
        }
    }
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len > 1e-3 {
        // 中间的带：投影落在线段上（0 ≤ t ≤ 1），并且离直线不超过 reach。两个条件各给出一段 x，取交集。
        let (mut l, mut h) = (f32::MIN, f32::MAX);
        let along = (cy - a.y) * dy;
        if dx.abs() > 1e-6 {
            let (x0, x1) = (a.x - along / dx, a.x - along / dx + len * len / dx);
            (l, h) = (l.max(x0.min(x1)), h.min(x0.max(x1)));
        } else if !(0.0..=len * len).contains(&along) {
            return finish(lo, hi);
        }
        if dy.abs() > 1e-6 {
            let (xc, w) = (a.x + (cy - a.y) * dx / dy, reach * len / dy.abs());
            (l, h) = (l.max(xc - w), h.min(xc + w));
        } else if ((cy - a.y) * dx / len).abs() > reach {
            return finish(lo, hi);
        }
        if l <= h {
            (lo, hi) = (lo.min(l), hi.max(h));
        }
    }
    finish(lo, hi)
}

fn finish(lo: f32, hi: f32) -> Option<(f32, f32)> {
    (lo <= hi).then_some((lo, hi))
}

/// 半径为 r 的圆在距圆心 d 处的覆盖率。
///
/// 硬边时过渡带宽一个像素，横截面的总覆盖量正好是直径 2r。
/// 半径不足半个像素时峰值到不了 1，再按比例压低，使总覆盖量仍是 2r：
/// 细线因此是渐渐变淡，而不是突然消失或停在某个最小粗细上。
/// 软边时从圆心向外平滑衰减，硬度越低衰减带越宽。衰减带的宽度与"1 − 硬度"的平方成正比：
/// 硬度 90% 时只有半径的 1%，500 像素的大笔刷边缘也只糊三四个像素；硬度往下调，软得越来越快，
/// 到 0 时从圆心一直软到边上。原先是一次方，大笔刷上硬度 90% 就糊出二十多个像素，
/// 能用的范围全挤在滑块最右边那一小段里（用户 2026-10-14 反映马克笔边缘太糊）。
fn coverage(r: f32, d: f32, hardness: f32) -> f32 {
    if r <= 0.0 {
        return 0.0;
    }
    if hardness >= 0.999 {
        let c = (r + 0.5 - d).clamp(0.0, 1.0);
        return if r < 0.5 { c * 2.0 * r / ((r + 0.5) * (r + 0.5)) } else { c };
    }
    let soft = (1.0 - hardness) * (1.0 - hardness);
    let c = ((r + 0.5 - d) / (r * soft + 1.0)).clamp(0.0, 1.0);
    let smooth = c * c * (3.0 - 2.0 * c);
    c + (smooth - c) * soft
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 沿 x = col 这一列把覆盖率加起来，得到该处的等效粗细（像素）。
    fn column_width(stroke: &Stroke, col: i32) -> f32 {
        let mut sum = 0.0;
        for y in -64i32..128 {
            let (tx, ty) = (col.div_euclid(TILE), y.div_euclid(TILE));
            if let Some(t) = stroke.mask.get((tx, ty)) {
                sum += t[((y - ty * TILE) * TILE + (col - tx * TILE)) as usize] as f32 / ONE as f32;
            }
        }
        sum
    }

    fn horizontal(params: BrushParams, pressure: impl Fn(f32) -> f32) -> Stroke {
        let at = |k: i32| Sample { x: 10.0 + k as f32 * 5.0, y: 30.5, p: pressure(k as f32 / 100.0) };
        let (mut s, _) = Stroke::begin(params, 0, at(0));
        for k in 1..=100 {
            s.add(at(k));
        }
        s.end();
        s
    }

    #[test]
    fn full_pressure_line_is_as_wide_as_the_brush() {
        let s = horizontal(BrushParams::default(), |_| 1.0);
        for col in [100, 250, 400] {
            assert!((column_width(&s, col) - 10.0).abs() < 0.15, "col {col}: {}", column_width(&s, col));
        }
    }

    /// 规格 F-011：粗细随压力线性、连续变化，相邻列的变化不超过 0.3 像素。
    #[test]
    fn width_follows_pressure_linearly_and_smoothly() {
        let s = horizontal(BrushParams::default(), |u| u);
        let mut prev = column_width(&s, 12);
        for col in 13..505 {
            let w = column_width(&s, col);
            assert!((w - prev).abs() <= 0.3, "step at col {col}: {prev} -> {w}");
            let want = 10.0 * ((col as f32 + 0.5 - 10.0) / 500.0).clamp(0.0, 1.0);
            assert!((w - want).abs() <= 0.5, "col {col}: got {w}, want {want}");
            prev = w;
        }
    }

    #[test]
    fn a_tap_leaves_a_dot_the_size_of_the_brush() {
        let (mut s, _) = Stroke::begin(BrushParams::default(), 3, Sample { x: 20.5, y: 20.5, p: 1.0 });
        s.end();
        // 圆点中间那一列的覆盖量应接近直径。
        assert!((column_width(&s, 20) - 10.0).abs() < 0.5);
    }

    #[test]
    fn stabilizer_gives_a_tapered_start() {
        let (mut s, _) = Stroke::begin(BrushParams::default(), 3, Sample { x: 10.0, y: 30.5, p: 1.0 });
        for k in 1..=40 {
            s.add(Sample { x: 10.0 + k as f32 * 5.0, y: 30.5, p: 1.0 });
        }
        s.end();
        assert!(column_width(&s, 11) < 4.0);
        assert!((column_width(&s, 100) - 10.0).abs() < 0.15);
    }
    fn value(stroke: &Stroke, x: i32, y: i32) -> f32 {
        let tc = (x.div_euclid(TILE), y.div_euclid(TILE));
        stroke.mask.get(tc).map_or(0.0, |t| t[((y - tc.1 * TILE) * TILE + (x - tc.0 * TILE)) as usize] as f32 / ONE as f32)
    }

    /// 曲线必须经过每个样本点：它是插值，不是又一层平滑（否则稳定器的验收值会被它拉偏）。
    #[test]
    fn curve_passes_through_the_samples() {
        let pts = [(20.5, 60.5), (60.5, 20.5), (100.5, 60.5), (140.5, 20.5), (180.5, 60.5)];
        let (mut s, _) = Stroke::begin(BrushParams { size: 4.0, ..Default::default() }, 0, Sample { x: pts[0].0, y: pts[0].1, p: 1.0 });
        for &(x, y) in &pts[1..] {
            s.add(Sample { x, y, p: 1.0 });
        }
        s.end();
        for &(x, y) in &pts {
            assert!(value(&s, x as i32, y as i32) > 0.99, "sample ({x}, {y}) is not covered");
        }
        // 急转弯处曲线不应甩出去很远：离折线 12 像素以外没有墨。
        assert_eq!(value(&s, 60, 5), 0.0);
        assert_eq!(value(&s, 100, 76), 0.0);
    }

    #[test]
    fn newest_segment_is_visible_before_the_next_sample_arrives() {
        let (mut s, _) = Stroke::begin(BrushParams::default(), 0, Sample { x: 10.5, y: 30.5, p: 1.0 });
        s.add(Sample { x: 60.5, y: 30.5, p: 1.0 });
        let (committed, tail) = s.masks((0, 0));
        let at = |m: Option<&[u16]>| m.map_or(0, |m| m[(30 * TILE + 40) as usize]);
        assert_eq!(at(committed), 0);
        assert_eq!(at(tail) as u32, ONE);
    }

    #[test]
    fn pressure_can_drive_density() {
        let params = BrushParams { min_density: 0.0, min_size: 1.0, ..Default::default() };
        let s = horizontal(params, |u| u);
        let centre = |col: i32| value(&s, col, 30);
        assert!((centre(260) - 0.5).abs() < 0.05, "{}", centre(260));
        assert!(centre(500) > 0.95);
        assert!(centre(40) < 0.1);
    }

    #[test]
    fn soft_brush_fades_from_the_centre() {
        let params = BrushParams { size: 40.0, hardness: 0.0, ..Default::default() };
        let s = horizontal(params, |_| 1.0);
        assert!(value(&s, 250, 30) > 0.9);
        let mid = value(&s, 250, 40);
        assert!(mid > 0.2 && mid < 0.8, "{mid}");
        assert!(value(&s, 250, 49) < 0.1);
        assert_eq!(value(&s, 250, 52), 0.0);
    }
    /// 边缘从"满"降到"空"要走多远（像素）。
    fn edge_width(r: f32, hardness: f32) -> f32 {
        let steps: Vec<f32> = (0..=4000).map(|k| k as f32 * 0.1).collect();
        let inner = steps.iter().rev().find(|&&d| coverage(r, d, hardness) >= 0.99).copied().unwrap_or(0.0);
        let outer = steps.iter().find(|&&d| coverage(r, d, hardness) <= 0.01).copied().unwrap_or(400.0);
        outer - inner
    }

    #[test]
    fn a_fairly_hard_big_brush_keeps_a_crisp_edge() {
        // 500 像素的笔：硬度 100% 是一个像素的抗锯齿边，90% 也只糊几个像素，往下调才越来越软。
        assert!(edge_width(250.0, 1.0) <= 1.1, "{}", edge_width(250.0, 1.0));
        assert!(edge_width(250.0, 0.9) < 5.0, "{}", edge_width(250.0, 0.9));
        let (w70, w30, w0) = (edge_width(250.0, 0.7), edge_width(250.0, 0.3), edge_width(250.0, 0.0));
        assert!(w70 > 10.0 && w30 > w70 && w0 > w30, "{w70} {w30} {w0}");
        // 小笔刷上差别不大：30 像素、硬度 90% 时边缘不到两个像素。
        assert!(edge_width(15.0, 0.9) < 2.0);
    }

    /// 落笔后原地停一下再画：点和线之间不能断开。
    #[test]
    fn no_gap_between_the_pen_down_dot_and_the_line() {
        for repeats in [0, 3, 12] {
            let at = |x: f32| Sample { x, y: 30.5, p: 0.5 };
            let (mut s, _) = Stroke::begin(BrushParams::default(), 3, at(10.5));
            for _ in 0..repeats {
                s.add(at(10.5));
            }
            for k in 1..=20 {
                s.add(at(10.5 + 20.0 * k as f32));
            }
            s.end();
            let widths: Vec<f32> = (12..300).map(|c| column_width(&s, c)).collect();
            let min = widths.iter().cloned().fold(f32::MAX, f32::min);
            assert!(min > 0.3, "repeats={repeats}: a column between the dot and the line is empty ({min}); widths start {:?}", &widths[..40]);
        }
    }
}
