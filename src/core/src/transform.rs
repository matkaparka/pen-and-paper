// SPDX-License-Identifier: GPL-3.0-or-later
//! 图层像素的仿射变换：移动、缩放、旋转、翻转。
//!
//! 像素 (x, y) 占据 [x, x+1)×[y, y+1)，中心在 (x+0.5, y+0.5)。重采样一律反向映射：
//! 把每个目标像素的中心映回源图，在那里插值。插值的是预乘后的值，透明像素的颜色不会渗进边缘。
//!
//! - 恰好把像素格映到像素格的变换（整数平移、翻转、90° 整数倍的旋转）不插值，逐像素照搬。
//! - `Fast`：双线性，定点运算。
//! - `Best`：Catmull-Rom 三次卷积（Keys 1981 的三次卷积核取 a = −0.5）。缩小时把每个目标像素
//!   分成一格子采样点取平均，格子的疏密跟着缩小的倍数走，细线才不会断成点。
//!
//! 有选区时，选中的部分先"浮"起来，变换后按通常的"源在上"（Porter & Duff 1984）叠回去。

use std::sync::Arc;

use crate::selection::Selection;
use crate::tile::{tiles_in, MaskMap, Tile, TileCoord, TileMap, TILE};
use crate::{Rect, ONE};

const N: usize = (TILE * TILE) as usize;
/// 一个分块里通道值的个数。
const LEN: usize = N * 4;
/// 分块边长是 2 的幂，由坐标取分块号和块内偏移时用移位和掩码。
const SHIFT: u32 = TILE.trailing_zeros();
const MASK: i64 = TILE as i64 - 1;
const _: () = assert!(TILE.count_ones() == 1);

/// 全透明的分块。源图里没有分块的位置都指向它，采样时不用再分情况。
static BLANK: [u16; LEN] = [0; LEN];
/// 采样函数收到的源坐标是 16.16 定点；这是换算时乘的倍数。
const FIX: f64 = 65536.0;
/// 沿一行累加源坐标时用 32.32 定点，步长的舍入误差累积 64 个像素也不到十亿分之一像素。
const FIXED_ONE: f64 = 4294967296.0;
/// 32.32 定点数放得下的坐标范围（留了一倍的余量）。
const FIXED_LIMIT: f64 = 1073741824.0;
/// `Best` 缩小时每个方向最多取多少个子采样点。各向同性地缩小时，子采样点的总数与源像素数相当，
/// 上限再高也不会更慢；它防的是一个方向压扁、另一个方向拉长的情况。
const MAX_SUBSAMPLES: i32 = 16;
/// 源图的分块表最多多少格。边长 16384 的画布只用得到 65536 格，超过这个数说明传进来的范围不对。
const MAX_GRID_CELLS: i64 = 1 << 22;

/// 仿射变换：x' = a·x + c·y + e，y' = b·x + d·y + f（把源坐标变到目标坐标，单位是画布像素）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Affine {
    pub const IDENTITY: Affine = Affine { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    pub fn translate(dx: f32, dy: f32) -> Self {
        Affine { e: dx, f: dy, ..Self::IDENTITY }
    }

    /// 以 (cx, cy) 为中心缩放
    pub fn scale_about(sx: f32, sy: f32, cx: f32, cy: f32) -> Self {
        Self::about(sx as f64, 0.0, 0.0, sy as f64, cx, cy)
    }

    /// 以 (cx, cy) 为中心旋转，弧度，正值在屏幕上为顺时针（y 轴向下）
    pub fn rotate_about(angle: f32, cx: f32, cy: f32) -> Self {
        let (mut s, mut c) = (angle as f64).sin_cos();
        // f32 表示不了精确的 π/2，直角算出来的 cos 是 1e-8 量级而不是 0，平移项跟着差一点。
        // 这里归整，直角的整数倍才能得到精确的矩阵、走照搬像素的那条路。
        if s.abs() < 1e-6 {
            (s, c) = (0.0, c.signum());
        } else if c.abs() < 1e-6 {
            (s, c) = (s.signum(), 0.0);
        }
        Self::about(c, s, -s, c, cx, cy)
    }

    /// 线性部分为 (a, b, c, d)、保持 (cx, cy) 不动的变换。
    fn about(a: f64, b: f64, c: f64, d: f64, cx: f32, cy: f32) -> Self {
        let (cx, cy) = (cx as f64, cy as f64);
        Mat64 { a, b, c, d, e: cx - a * cx - c * cy, f: cy - b * cx - d * cy }.narrow()
    }

    /// 先做 self，再做 next
    pub fn then(&self, next: &Affine) -> Affine {
        let (s, n) = (Mat64::of(self), Mat64::of(next));
        Mat64 {
            a: n.a * s.a + n.c * s.b,
            b: n.b * s.a + n.d * s.b,
            c: n.a * s.c + n.c * s.d,
            d: n.b * s.c + n.d * s.d,
            e: n.a * s.e + n.c * s.f + n.e,
            f: n.b * s.e + n.d * s.f + n.f,
        }
        .narrow()
    }

    /// 逆变换。矩阵奇异（把平面压成了一条线或一个点）或含有非有限值时没有逆。
    pub fn inverse(&self) -> Option<Affine> {
        let m = Mat64::of(self).inverse()?.narrow();
        [m.a, m.b, m.c, m.d, m.e, m.f].iter().all(|v| v.is_finite()).then_some(m)
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let (u, v) = Mat64::of(self).apply(x as f64, y as f64);
        (u as f32, v as f32)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    /// 双线性，拖动时的实时预览用
    Fast,
    /// 双三次（Catmull-Rom），缩小时逐像素超采样；确认时用
    Best,
}

/// 内部计算用的双精度矩阵。f32 在坐标 16384 附近只能分辨到千分之一像素，连乘和求逆要用更宽的类型。
#[derive(Clone, Copy)]
struct Mat64 {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Mat64 {
    fn of(m: &Affine) -> Self {
        Mat64 { a: m.a as f64, b: m.b as f64, c: m.c as f64, d: m.d as f64, e: m.e as f64, f: m.f as f64 }
    }

    fn narrow(&self) -> Affine {
        Affine { a: self.a as f32, b: self.b as f32, c: self.c as f32, d: self.d as f32, e: self.e as f32, f: self.f as f32 }
    }

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f)
    }

    fn inverse(&self) -> Option<Mat64> {
        let det = self.a * self.d - self.b * self.c;
        // 行列式和它的两项相比小到只剩舍入误差，就当作奇异。全零矩阵两边都是 0，也落在这里。
        let scale = (self.a * self.d).abs() + (self.b * self.c).abs();
        if !det.is_finite() || det.abs() <= scale * 1e-12 || !self.e.is_finite() || !self.f.is_finite() {
            return None;
        }
        let (a, b, c, d) = (self.d / det, -self.b / det, -self.c / det, self.a / det);
        Some(Mat64 { a, b, c, d, e: -(a * self.e + c * self.f), f: -(b * self.e + d * self.f) })
    }
}

/// 恰好把像素格映到像素格的变换：目标像素下标 = A·源像素下标 + T，A 的元素只有 0 和 ±1。
#[derive(Clone, Copy)]
struct GridMap {
    a: i64,
    b: i64,
    c: i64,
    d: i64,
    tx: i64,
    ty: i64,
}

impl GridMap {
    fn snap(m: &Affine) -> Option<GridMap> {
        let unit = |v: f32| [-1i64, 0, 1].into_iter().find(|&k| (v - k as f32).abs() <= 1e-4);
        let (a, b, c, d) = (unit(m.a)?, unit(m.b)?, unit(m.c)?, unit(m.d)?);
        // 每行、每列恰好一个 ±1，才是坐标轴的对换或翻转。
        if a.abs() + c.abs() != 1 || b.abs() + d.abs() != 1 || a.abs() + b.abs() != 1 {
            return None;
        }
        let whole = |v: f32| ((v - v.round()).abs() <= 1e-3 && v.abs() < 1e9).then(|| v.round() as i64);
        // 像素中心在下标 +0.5 处。系数为 −1 的那个轴上，中心 i+0.5 映到 t−i−0.5，
        // 也就是下标 t−i−1 的像素，比平移项少 1。
        Some(GridMap { a, b, c, d, tx: whole(m.e)? + (a + c - 1) / 2, ty: whole(m.f)? + (b + d - 1) / 2 })
    }

    fn is_identity(&self) -> bool {
        (self.a, self.d, self.tx, self.ty) == (1, 1, 0, 0)
    }

    fn forward(&self, x: i64, y: i64) -> (i64, i64) {
        (self.a * x + self.c * y + self.tx, self.b * x + self.d * y + self.ty)
    }

    /// A 是正交的，逆就是转置。
    fn backward(&self, x: i64, y: i64) -> (i64, i64) {
        let (x, y) = (x - self.tx, y - self.ty);
        (self.a * x + self.b * y, self.c * x + self.d * y)
    }
}

/// 非透明像素的最紧外接矩形（裁到 clip 内）；全透明时返回 Rect::EMPTY。
pub fn content_bounds(tiles: &TileMap, clip: Rect) -> Rect {
    let mut b = Rect::EMPTY;
    for tc in tiles.coords() {
        let r = tile_rect_in(tc, clip);
        // 整块都已经在范围里的分块扩大不了范围，不用看。内容连成一片时大部分分块都这样跳过。
        if r.is_empty() || (r.x0 >= b.x0 && r.y0 >= b.y0 && r.x1 <= b.x1 && r.y1 <= b.y1) {
            continue;
        }
        let Some(t) = tiles.get(tc).and_then(|t| pixels(t)) else { continue };
        for y in r.y0..r.y1 {
            let row = t[row_range(tc, r, y)].as_chunks::<4>().0;
            let Some(first) = row.iter().position(|p| p[3] != 0) else { continue };
            let last = row.iter().rposition(|p| p[3] != 0).unwrap_or(first);
            b = b.union(Rect { x0: r.x0 + first as i32, y0: y, x1: r.x0 + last as i32 + 1, y1: y + 1 });
        }
    }
    b
}

/// 把图层像素按 m 变换，结果裁到 clip（画布范围）内。
/// 有选区时只变换选中的部分：结果 = 源 ×（1 − 选区）留在原地 + 变换后的（源 × 选区）叠在上面。
///
/// clip 之外的源像素当作透明：没有选区时它们不进结果；有选区时它们不算选中，原样留着。
/// m 奇异时没法变换，返回源的拷贝。
pub fn transform_tiles(src: &TileMap, selection: Option<&Selection>, m: &Affine, quality: Quality, clip: Rect) -> TileMap {
    let Some(inv) = Mat64::of(m).inverse() else { return src.clone() };
    let exact = GridMap::snap(m);
    let identity = exact.is_some_and(|g| g.is_identity());
    // 软边选区浮起来再原地落回去，过渡带上的不透明度会掉一点（"源在上"对互补的两半并不还原）。
    // 什么都没动就应该什么都不变，所以恒等变换不走那一遍。
    if identity && selection.is_some() {
        return src.clone();
    }
    let (floating, rest) = lift(src, selection, clip);
    let bounds = content_bounds(&floating, clip);
    let moved = if bounds.is_empty() {
        TileMap::default()
    } else if identity {
        floating
    } else {
        let Some(grid) = Grid::new(&floating, bounds) else { return src.clone() };
        match exact {
            Some(g) => copy_exact(&grid, bounds, &g, clip),
            None => resample(&grid, bounds, m, &inv, quality, clip),
        }
    };
    if selection.is_some() {
        over(rest, moved)
    } else {
        moved
    }
}

/// 让选区跟着内容一起变换。
///
/// 覆盖率当作一张只有透明度的图，走和像素相同的路（`Best`），所以选区的边缘和确认变换后
/// 内容的边缘是对得上的。代价是多处理三个空通道；这个函数只在确认时调用一次，不在乎。
pub fn transform_selection(sel: &Selection, m: &Affine, clip: Rect) -> Selection {
    if Mat64::of(m).inverse().is_none() {
        return Selection::empty();
    }
    let image = transform_tiles(&selection_image(sel), None, m, Quality::Best, clip);
    image_selection(&image, clip)
}

/// 选区的覆盖率当作一张只有透明度的图。
fn selection_image(sel: &Selection) -> TileMap {
    let mut image = TileMap::default();
    for tc in tiles_in(sel.bounds()) {
        let Some(s) = sel.tile(tc).filter(|s| s.iter().any(|&v| v != 0)) else { continue };
        for (px, &v) in image.get_mut(tc).0.as_chunks_mut::<4>().0.iter_mut().zip(s) {
            px[3] = v.min(ONE as u16);
        }
    }
    image
}

/// 反过来，把一张图的透明度当作选区。
fn image_selection(image: &TileMap, clip: Rect) -> Selection {
    let bounds = content_bounds(image, clip);
    let mut mask = MaskMap::default();
    for tc in tiles_in(bounds) {
        let Some(t) = image.get(tc) else { continue };
        for (v, px) in mask.get_mut(tc).iter_mut().zip(t.0.as_chunks::<4>().0) {
            *v = px[3];
        }
    }
    Selection::from_mask(mask, bounds)
}

/// 不是仿射的映射（实现方案 §3.7）：说出每个目标像素的中心从源图上的哪一点取色。
/// 透视、网格变形、液化各自实现它（见 `warp`、`liquify`），重采样都在这里。
pub trait Mapping: Sync {
    /// 结果可能有内容的范围（画布坐标）。这之外的目标像素不去算。
    fn reach(&self) -> Rect;

    /// 矩形 `r`（不跨分块）里各像素中心对应的源坐标，逐行写进 `out`：每行 w + 1 个，共 h + 1 行
    ///（w、h 是 `r` 的宽高）。多出来的一列一行是紧挨着 `r` 右边、下边的像素，用来看相邻像素之间
    /// 源坐标差多少，也就是这里缩小了多少倍。
    /// 坐标是连续的画布坐标（像素 (x, y) 占 [x, x+1)）。进来时 `out` 里全是 NaN，没有对应源点的就留着。
    fn sources(&self, r: Rect, out: &mut [(f64, f64)]);
}

/// 按一般的映射变换图层像素；选区和画布范围的处理与 `transform_tiles` 相同。
pub fn map_tiles(src: &TileMap, selection: Option<&Selection>, m: &dyn Mapping, quality: Quality, clip: Rect) -> TileMap {
    let (floating, rest) = lift(src, selection, clip);
    let bounds = content_bounds(&floating, clip);
    let moved = if bounds.is_empty() {
        TileMap::default()
    } else {
        let Some(grid) = Grid::new(&floating, bounds) else { return src.clone() };
        let dest = m.reach().intersect(clip);
        build(dest, |tc, scratch| mapped(&grid, bounds, m, quality, tile_rect_in(tc, dest), tc, scratch))
    };
    if selection.is_some() {
        over(rest, moved)
    } else {
        moved
    }
}

/// 选区跟着一般的映射走，做法同 `transform_selection`。
pub fn map_selection(sel: &Selection, m: &dyn Mapping, clip: Rect) -> Selection {
    let image = map_tiles(&selection_image(sel), None, m, Quality::Best, clip);
    image_selection(&image, clip)
}

/// 只重算给出的这些分块（液化每动一下只重算笔刷碰到的地方）。整张 `src` 都是源图，不分选区；
/// `from` 是可能被取到的源范围，范围外当作透明。返回这些分块（画布内的部分）的新内容，全透明的是 None。
pub fn map_some(src: &TileMap, m: &dyn Mapping, quality: Quality, clip: Rect, tiles: &[TileCoord], from: Rect) -> Vec<(TileCoord, Option<Arc<Tile>>)> {
    let from = from.intersect(clip);
    let done = match Grid::new(src, from) {
        Some(grid) if !from.is_empty() => build_tiles(tiles, |tc, scratch| mapped(&grid, from, m, quality, tile_rect_in(tc, clip), tc, scratch)),
        _ => TileMap::default(),
    };
    tiles.iter().map(|&tc| (tc, done.get(tc).cloned())).collect()
}

/// 按映射算出目标分块里矩形 `r` 的像素，写进 `scratch`。返回有没有写进看得见的像素。
/// `bounds` 是源图上有内容的范围（画布坐标）。
fn mapped(grid: &Grid, bounds: Rect, m: &dyn Mapping, quality: Quality, r: Rect, tc: TileCoord, scratch: &mut [u16]) -> bool {
    if r.is_empty() {
        return false;
    }
    let w = (r.x1 - r.x0) as usize;
    let mut at = vec![(f64::NAN, f64::NAN); (w + 1) * ((r.y1 - r.y0) as usize + 1)];
    m.sources(r, &mut at);
    let radius = if quality == Quality::Fast { 1.0 } else { 2.0 };
    // 下面的源坐标以"像素下标"为单位（连续坐标减 0.5），并相对表的左上角。
    let (lo_x, hi_x) = ((bounds.x0 as i64 - grid.ox) as f64 - radius, (bounds.x1 as i64 - 1 - grid.ox) as f64 + radius);
    let (lo_y, hi_y) = ((bounds.y0 as i64 - grid.oy) as f64 - radius, (bounds.y1 as i64 - 1 - grid.oy) as f64 + radius);
    // 四舍五入到 16.16 定点。不用 `round`：没有指定处理器型号时它是一次函数调用，逐像素用太慢。
    let fixed = |v: f64| floor(v * FIX + 0.5);
    let mut any = false;
    for y in r.y0..r.y1 {
        let j = (y - r.y0) as usize;
        let row = &mut scratch[row_range(tc, r, y)];
        for (i, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (u, v) = at[j * (w + 1) + i];
            if !u.is_finite() || !v.is_finite() {
                continue;
            }
            let x = r.x0 + i as i32;
            let (sx, sy) = (u - 0.5 - grid.ox as f64, v - 0.5 - grid.oy as f64);
            let p = if u == x as f64 + 0.5 && v == y as f64 + 0.5 {
                // 映到自己身上的像素原样照搬（液化里没被碰到的地方），一位不差。
                grid.px(x as i64 - grid.ox, y as i64 - grid.oy)
            } else if quality == Quality::Fast {
                if sx < lo_x || sx > hi_x || sy < lo_y || sy > hi_y {
                    continue;
                }
                grid.bilinear(fixed(sx), fixed(sy))
            } else {
                // 相邻像素的源坐标差多少，就是这里的局部缩放；邻居没有源点时退回去看另一边的邻居。
                let step = |a: (f64, f64), b: Option<(f64, f64)>, c: Option<(f64, f64)>, unit: (f64, f64)| match (b, c) {
                    (Some(b), _) if b.0.is_finite() && b.1.is_finite() => (b.0 - a.0, b.1 - a.1),
                    (_, Some(c)) if c.0.is_finite() && c.1.is_finite() => (a.0 - c.0, a.1 - c.1),
                    _ => unit,
                };
                let k = j * (w + 1) + i;
                let du = step((u, v), at.get(k + 1).copied(), i.checked_sub(1).map(|_| at[k - 1]), (1.0, 0.0));
                let dv = step((u, v), at.get(k + w + 1).copied(), j.checked_sub(1).map(|_| at[k - w - 1]), (0.0, 1.0));
                let (nx, ny) = (subsamples(du.0.hypot(du.1)), subsamples(dv.0.hypot(dv.1)));
                let (hx, hy) = (0.5 * (du.0.abs() + dv.0.abs()), 0.5 * (du.1.abs() + dv.1.abs()));
                if sx + hx < lo_x || sx - hx > hi_x || sy + hy < lo_y || sy - hy > hi_y {
                    continue;
                }
                let mut acc = [0f32; 4];
                for b in 0..ny {
                    for a in 0..nx {
                        let (s, t) = ((a as f64 + 0.5) / nx as f64 - 0.5, (b as f64 + 0.5) / ny as f64 - 0.5);
                        grid.bicubic(fixed(sx + s * du.0 + t * dv.0), fixed(sy + s * du.1 + t * dv.1), &mut acc);
                    }
                }
                finish(acc, 1.0 / (nx * ny) as f32)
            };
            if p[3] != 0 {
                *px = p;
                any = true;
            }
        }
    }
    any
}

/// 把要变换的像素"浮"起来，返回（浮起来的那一块，留在原地的部分）。没有选区时整层都浮起来。
fn lift(src: &TileMap, selection: Option<&Selection>, clip: Rect) -> (TileMap, TileMap) {
    let mut floating = TileMap::default();
    let Some(sel) = selection else {
        for tc in src.coords() {
            let r = tile_rect_in(tc, clip);
            let Some((whole, t)) = src.get(tc).and_then(|a| Some((a, pixels(a)?))) else { continue };
            if r.x1 - r.x0 == TILE && r.y1 - r.y0 == TILE {
                // 整块都在画布里，直接共用，不复制。
                floating.replace(tc, Some(whole.clone()));
            } else if !r.is_empty() {
                let mut cut = vec![0u16; LEN];
                for y in r.y0..r.y1 {
                    let row = row_range(tc, r, y);
                    cut[row.clone()].copy_from_slice(&t[row]);
                }
                if visible(&cut) {
                    floating.replace(tc, Some(Arc::new(Tile(cut))));
                }
            }
        }
        return (floating, TileMap::default());
    };
    // 选区碰不到的分块在 rest 里仍与源共用，一个像素都不会动。
    let mut rest = src.clone();
    for tc in tiles_in(sel.bounds().intersect(clip)) {
        let (Some(t), Some(s)) = (src.get(tc).and_then(|t| pixels(t)), sel.tile(tc)) else { continue };
        let r = tile_rect_in(tc, clip);
        let mut split: Option<(Vec<u16>, Vec<u16>)> = None;
        for y in r.y0..r.y1 {
            let row = row_range(tc, r, y);
            for i in row.start / 4..row.end / 4 {
                let v = (s.get(i).copied().unwrap_or(0) as u32).min(ONE);
                if v == 0 || t[i * 4 + 3] == 0 {
                    continue;
                }
                let (up, down) = split.get_or_insert_with(|| (vec![0; LEN], t.to_vec()));
                for c in i * 4..i * 4 + 4 {
                    up[c] = mul15(t[c] as u32, v) as u16;
                    down[c] = mul15(t[c] as u32, ONE - v) as u16;
                }
            }
        }
        if let Some((up, down)) = split {
            floating.replace(tc, Some(Arc::new(Tile(up))));
            rest.replace(tc, visible(&down).then(|| Arc::new(Tile(down))));
        }
    }
    (floating, rest)
}

/// 把 top 叠在 base 上面：结果 = 上 + 下 ×（1 − 上的不透明度），都是预乘值。
fn over(mut base: TileMap, top: TileMap) -> TileMap {
    for tc in top.coords() {
        let Some((whole, t)) = top.get(tc).and_then(|a| Some((a, pixels(a)?))) else { continue };
        if base.get(tc).is_none() {
            base.replace(tc, Some(whole.clone()));
            continue;
        }
        for (d, s) in base.get_mut(tc).0.as_chunks_mut::<4>().0.iter_mut().zip(t.as_chunks::<4>().0) {
            let keep = ONE - (s[3] as u32).min(ONE);
            if keep == ONE {
                continue;
            }
            let a = (s[3] as u32 + mul15(d[3] as u32, keep)).min(ONE);
            // 两项各自四舍五入，加起来可能比透明度多出 1。
            for c in 0..3 {
                d[c] = (s[c] as u32 + mul15(d[c] as u32, keep)).min(a) as u16;
            }
            d[3] = a as u16;
        }
    }
    base
}

/// 重采样时的源图：内容范围内的分块排成一张表，采样时由坐标直接算出下标，不查哈希。
///
/// 没有把像素摊平到一整块缓冲区里：那样每像素 8 字节，16384×16384 的画布要 2 GB。
/// 这张表每个分块只占一个指针，同样大的画布是 512 KB。
struct Grid<'a> {
    /// 表左上角的画布坐标
    ox: i64,
    oy: i64,
    /// 表的宽、高，以分块计
    w: u64,
    h: u64,
    cells: Vec<&'a [u16; LEN]>,
}

impl<'a> Grid<'a> {
    fn new(tiles: &'a TileMap, bounds: Rect) -> Option<Self> {
        let t = TILE as i64;
        let (tx0, ty0) = ((bounds.x0 as i64).div_euclid(t), (bounds.y0 as i64).div_euclid(t));
        let (w, h) = ((bounds.x1 as i64 - 1).div_euclid(t) - tx0 + 1, (bounds.y1 as i64 - 1).div_euclid(t) - ty0 + 1);
        if w * h > MAX_GRID_CELLS {
            return None;
        }
        let mut cells: Vec<&'a [u16; LEN]> = vec![&BLANK; (w * h) as usize];
        for tc in tiles_in(bounds) {
            if let Some(p) = tiles.get(tc).and_then(|t| pixels(t)) {
                cells[((tc.1 as i64 - ty0) * w + (tc.0 as i64 - tx0)) as usize] = p;
            }
        }
        Some(Grid { ox: tx0 * t, oy: ty0 * t, w: w as u64, h: h as u64, cells })
    }

    /// (x, y) 所在的分块。坐标相对表的左上角，出了表就没有。
    #[inline]
    fn cell(&self, x: i64, y: i64) -> Option<&'a [u16; LEN]> {
        // 负的分块号转成无符号数是个极大的值，和超出右、下边界的一起被同一个比较挡掉。
        let (tx, ty) = ((x >> SHIFT) as u64, (y >> SHIFT) as u64);
        if tx >= self.w || ty >= self.h {
            return None;
        }
        self.cells.get((ty * self.w + tx) as usize).copied()
    }

    #[inline]
    fn px(&self, x: i64, y: i64) -> [u16; 4] {
        match self.cell(x, y) {
            Some(t) => rgba(t, ((((y & MASK) << SHIFT) | (x & MASK)) << 2) as usize),
            None => [0; 4],
        }
    }

    /// 在 16.16 定点坐标 (x, y) 处双线性插值。整数部分是左上那个像素的下标。
    #[inline]
    fn bilinear(&self, x: i64, y: i64) -> [u16; 4] {
        let (ix, iy) = (x >> 16, y >> 16);
        // 权重取 15 位，和通道值的定点位数一致。
        let (fx, fy) = ((x & 0xFFFF) as u64 >> 1, (y & 0xFFFF) as u64 >> 1);
        let (lx, ly) = ((ix & MASK) as usize, (iy & MASK) as usize);
        let edge = TILE as usize - 1;
        let p = if lx < edge && ly < edge {
            // 四个像素在同一个分块里：只找一次分块。
            let Some(t) = self.cell(ix, iy) else { return [0; 4] };
            if std::ptr::eq(t, &BLANK) {
                return [0; 4];
            }
            let o = (ly * TILE as usize + lx) * 4;
            let (top, bottom) = (&t[o..o + 8], &t[o + TILE as usize * 4..o + TILE as usize * 4 + 8]);
            [rgba(top, 0), rgba(top, 4), rgba(bottom, 0), rgba(bottom, 4)]
        } else {
            [self.px(ix, iy), self.px(ix + 1, iy), self.px(ix, iy + 1), self.px(ix + 1, iy + 1)]
        };
        // 一个像素拼成一个 64 位整数，隔一个通道取一个就是两组"各占 32 位"的通道，一次乘法算两个。
        let [p00, p10, p01, p11] = p.map(|q| q[0] as u64 | ((q[1] as u64) << 16) | ((q[2] as u64) << 32) | ((q[3] as u64) << 48));
        let mix = |shift: u32| {
            let pair = |v: u64| (v >> shift) & PAIR;
            lerp2(lerp2(pair(p00), pair(p10), fx), lerp2(pair(p01), pair(p11), fx), fy)
        };
        // 四个通道用的是同样的权重、同样的舍入，颜色 ≤ 透明度 ≤ ONE 的关系插值之后仍然成立。
        let v = mix(0) | (mix(16) << 16);
        [v as u16, (v >> 16) as u16, (v >> 32) as u16, (v >> 48) as u16]
    }

    /// 在 16.16 定点坐标 (x, y) 处做一次 Catmull-Rom 采样，加到 acc 上。
    #[inline]
    fn bicubic(&self, x: i64, y: i64, acc: &mut [f32; 4]) {
        let (ix, iy) = (x >> 16, y >> 16);
        let (lx, ly) = ((ix & MASK) as usize, (iy & MASK) as usize);
        let inner = 1..TILE as usize - 2;
        // 4×4 个像素都在同一个分块里时只找一次分块，否则逐个像素去找。
        let tile = if inner.contains(&lx) && inner.contains(&ly) {
            match self.cell(ix, iy) {
                Some(t) if !std::ptr::eq(t, &BLANK) => Some(t),
                _ => return,
            }
        } else {
            None
        };
        let wx = catmull_rom((x & 0xFFFF) as f32 / FIX as f32);
        let wy = catmull_rom((y & 0xFFFF) as f32 / FIX as f32);
        for (j, &w) in wy.iter().enumerate() {
            let p = match tile {
                Some(t) => {
                    let o = ((ly + j - 1) * TILE as usize + lx - 1) * 4;
                    let row = &t[o..o + 16];
                    [rgba(row, 0), rgba(row, 4), rgba(row, 8), rgba(row, 12)]
                }
                None => {
                    let y = iy + j as i64 - 1;
                    [self.px(ix - 1, y), self.px(ix, y), self.px(ix + 1, y), self.px(ix + 2, y)]
                }
            };
            for (c, o) in acc.iter_mut().enumerate() {
                *o += w * (wx[0] * p[0][c] as f32 + wx[1] * p[1][c] as f32 + wx[2] * p[2][c] as f32 + wx[3] * p[3][c] as f32);
            }
        }
    }
}

/// 照搬像素：每个目标像素直接取它对应的那个源像素。
fn copy_exact(grid: &Grid, bounds: Rect, g: &GridMap, clip: Rect) -> TileMap {
    let p = g.forward(bounds.x0 as i64, bounds.y0 as i64);
    let q = g.forward(bounds.x1 as i64 - 1, bounds.y1 as i64 - 1);
    let fit = |v: i64| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let dest = Rect { x0: fit(p.0.min(q.0)), y0: fit(p.1.min(q.1)), x1: fit(p.0.max(q.0) + 1), y1: fit(p.1.max(q.1) + 1) }.intersect(clip);
    build(dest, |tc, scratch| {
        let r = tile_rect_in(tc, dest);
        let mut any = false;
        for y in r.y0..r.y1 {
            let (sx, sy) = g.backward(r.x0 as i64, y as i64);
            let (mut sx, mut sy) = (sx - grid.ox, sy - grid.oy);
            for px in scratch[row_range(tc, r, y)].as_chunks_mut::<4>().0 {
                let p = grid.px(sx, sy);
                if p[3] != 0 {
                    *px = p;
                    any = true;
                }
                (sx, sy) = (sx + g.a, sy + g.c);
            }
        }
        any
    })
}

/// 一般的仿射变换：逐个目标像素反向映射、插值。
fn resample(grid: &Grid, bounds: Rect, m: &Affine, inv: &Mat64, quality: Quality, clip: Rect) -> TileMap {
    // 插值核的半径（源像素），以及一个目标像素里横竖各分几个子采样点。
    let (radius, nx, ny) = match quality {
        Quality::Fast => (1.0, 1, 1),
        Quality::Best => (2.0, subsamples(inv.a.hypot(inv.b)), subsamples(inv.c.hypot(inv.d))),
    };
    // 各子采样点相对像素中心的偏移，换算到源图里；hx、hy 是其中最远的。
    let mut offsets = Vec::with_capacity((nx * ny) as usize);
    let (mut hx, mut hy) = (0f64, 0f64);
    for j in 0..ny {
        for i in 0..nx {
            let (u, v) = ((i as f64 + 0.5) / nx as f64 - 0.5, (j as f64 + 0.5) / ny as f64 - 0.5);
            let (dx, dy) = (inv.a * u + inv.c * v, inv.b * u + inv.d * v);
            offsets.push(((dx * FIX).round() as i64, (dy * FIX).round() as i64));
            (hx, hy) = (hx.max(dx.abs()), hy.max(dy.abs()));
        }
    }
    let norm = 1.0 / (nx * ny) as f32;

    // 下面的源坐标都以"像素下标"为单位（连续坐标减 0.5，整数处正是像素中心），并相对表的左上角。
    // 采样点落在这个范围之外时，核够不着任何有内容的像素，结果必然透明。
    let (lo_x, hi_x) = ((bounds.x0 as i64 - grid.ox) as f64 - radius - hx, (bounds.x1 as i64 - 1 - grid.ox) as f64 + radius + hx);
    let (lo_y, hi_y) = ((bounds.y0 as i64 - grid.oy) as f64 - radius - hy, (bounds.y1 as i64 - 1 - grid.oy) as f64 + radius + hy);

    // 目标范围：内容的外接矩形向外扩一个核半径，映到目标后再取外接矩形。多留的一个像素是给子采样点的。
    let (x0, x1) = (bounds.x0 as f64 + 0.5 - radius, bounds.x1 as f64 - 0.5 + radius);
    let (y0, y1) = (bounds.y0 as f64 + 0.5 - radius, bounds.y1 as f64 - 0.5 + radius);
    let fwd = Mat64::of(m);
    let (u0, u1, v0, v1) = extent([fwd.apply(x0, y0), fwd.apply(x1, y0), fwd.apply(x0, y1), fwd.apply(x1, y1)]);
    let dest = Rect {
        x0: (u0.floor() as i32).saturating_sub(1),
        y0: (v0.floor() as i32).saturating_sub(1),
        x1: (u1.ceil() as i32).saturating_add(1),
        y1: (v1.ceil() as i32).saturating_add(1),
    }
    .intersect(clip);

    // 目标像素下标 → 源像素下标：把两头像素中心的 ±0.5 和表的原点都并进平移项。
    let to_src = Mat64 {
        e: inv.e + 0.5 * (inv.a + inv.c) - 0.5 - grid.ox as f64,
        f: inv.f + 0.5 * (inv.b + inv.d) - 0.5 - grid.oy as f64,
        ..*inv
    };
    build(dest, |tc, scratch| {
        let r = tile_rect_in(tc, dest);
        // 分块的四角映回源图，外接矩形碰不到内容就整块跳过（旋转后的外接矩形有四个空角）。
        let (rx, ry, rx1, ry1) = (r.x0 as f64, r.y0 as f64, (r.x1 - 1) as f64, (r.y1 - 1) as f64);
        let (sx0, sx1, sy0, sy1) = extent([to_src.apply(rx, ry), to_src.apply(rx1, ry), to_src.apply(rx, ry1), to_src.apply(rx1, ry1)]);
        if sx1 < lo_x || sx0 > hi_x || sy1 < lo_y || sy0 > hi_y {
            return false;
        }
        let mut any = false;
        for y in r.y0..r.y1 {
            // 这一行里只有中间一段映得到内容附近，两头直接跳过。
            let start = to_src.apply(rx, y as f64);
            let (ax, bx) = span(start.0, inv.a, lo_x, hi_x);
            let (ay, by) = span(start.1, inv.b, lo_y, hi_y);
            let (k0, k1) = (ax.max(ay).max(0), bx.min(by).min((r.x1 - r.x0) as i64));
            if k0 >= k1 {
                continue;
            }
            let start = (start.0 + k0 as f64 * inv.a, start.1 + k0 as f64 * inv.b);
            let row = &mut scratch[row_range(tc, r, y)][k0 as usize * 4..k1 as usize * 4];
            any |= match quality {
                Quality::Fast => fill_row(row, start, (inv.a, inv.b), |sx, sy| grid.bilinear(sx, sy)),
                Quality::Best => fill_row(row, start, (inv.a, inv.b), |sx, sy| {
                    let mut acc = [0f32; 4];
                    for &(dx, dy) in &offsets {
                        grid.bicubic(sx.wrapping_add(dx), sy.wrapping_add(dy), &mut acc);
                    }
                    finish(acc, norm)
                }),
            };
        }
        any
    })
}

/// 沿目标的一行逐像素采样，`start` 是第一个像素的源坐标，`step` 是每走一个像素源坐标的变化。
/// 返回这一行有没有写进看得见的像素。
#[inline]
fn fill_row(row: &mut [u16], start: (f64, f64), step: (f64, f64), mut sample: impl FnMut(i64, i64) -> [u16; 4]) -> bool {
    // 行内用 32.32 定点一路加过去，省掉每个像素两次浮点转整数。先确认这一行两头的坐标定点数都放得下
    // （中间的自然也放得下）。放不下的行离内容有十亿个像素远，只在缩小上亿倍时出现，本来也采不到东西。
    let last = (row.len() / 4) as f64 - 1.0;
    let ends = [start.0, start.1, start.0 + last * step.0, start.1 + last * step.1];
    if !ends.iter().all(|v| v.abs() < FIXED_LIMIT) {
        return false;
    }
    let fixed = |v: f64| (v * FIXED_ONE) as i64;
    let (mut x, mut y) = (fixed(start.0), fixed(start.1));
    let (dx, dy) = (fixed(step.0), fixed(step.1));
    let mut any = false;
    for px in row.as_chunks_mut::<4>().0 {
        let p = sample(x >> 16, y >> 16);
        if p[3] != 0 {
            *px = p;
            any = true;
        }
        // 只有一个像素的行，步长可能大得离谱；那时加出来的值不会再用到，绕回去也无妨。
        (x, y) = (x.wrapping_add(dx), y.wrapping_add(dy));
    }
    any
}

/// 向下取整。`f64::floor` 在没有指定处理器型号时是一次函数调用，逐像素用太慢；这里只截掉小数再修正负数。
/// 只用于取样坐标，调用的地方已经保证它在 i64 的范围里。
#[inline]
fn floor(v: f64) -> i64 {
    let i = v as i64;
    if (i as f64) > v {
        i - 1
    } else {
        i
    }
}

/// 目标分块不少于这么多时，分给几个线程去算。拖手柄时每动一下都要重算一遍预览，大图层上单线程跟不上手。
const PARALLEL_TILES: usize = 48;

/// 逐个算出 `dest` 里的目标分块。`fill` 把一个分块的像素写进给它的缓冲区（进来时是全零），
/// 返回里面有没有看得见的像素；有才收进结果，所以结果里不会有全透明的分块。
/// 各分块互不相干，分给几个线程算和一个线程算，结果逐像素相同。
fn build(dest: Rect, fill: impl Fn(TileCoord, &mut [u16]) -> bool + Sync) -> TileMap {
    build_tiles(&tiles_in(dest).collect::<Vec<_>>(), fill)
}

/// 同 `build`，算的是给出的这些分块。
fn build_tiles(coords: &[TileCoord], fill: impl Fn(TileCoord, &mut [u16]) -> bool + Sync) -> TileMap {
    let work = |part: &[TileCoord]| {
        let mut scratch = vec![0u16; LEN];
        let mut done: Vec<(TileCoord, Arc<Tile>)> = Vec::new();
        for &tc in part {
            if fill(tc, &mut scratch) {
                // 写好的缓冲区直接交出去，换一块干净的接着用。
                done.push((tc, Arc::new(Tile(std::mem::replace(&mut scratch, vec![0; LEN])))));
            }
        }
        done
    };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let parts: Vec<Vec<(TileCoord, Arc<Tile>)>> = if coords.len() >= PARALLEL_TILES && threads > 1 {
        let work = &work;
        std::thread::scope(|scope| {
            let jobs: Vec<_> = coords.chunks(coords.len().div_ceil(threads)).map(|part| scope.spawn(move || work(part))).collect();
            jobs.into_iter().map(|j| j.join().unwrap()).collect()
        })
    } else {
        vec![work(coords)]
    };
    let mut out = TileMap::default();
    for (tc, tile) in parts.into_iter().flatten() {
        out.replace(tc, Some(tile));
    }
    out
}

/// 目标上每走一个像素、源图里走过 len 个像素时，这个方向分几个子采样点：
/// 让相邻两点在源图里相距不超过一个像素，核的支撑连起来才没有漏掉的源像素。
fn subsamples(len: f64) -> i32 {
    // 留一点余量：纯旋转算出来的 1.0000001 不该多采一倍。
    ((len - 0.01).ceil() as i32).clamp(1, MAX_SUBSAMPLES)
}

/// 一行里 k 的范围 [k0, k1)，使 s0 + k·step 落在 [lo, hi] 里。两头各放宽一格，宁多勿少。
fn span(s0: f64, step: f64, lo: f64, hi: f64) -> (i64, i64) {
    if step.abs() < 1e-9 {
        return if s0 >= lo && s0 <= hi { (0, i64::MAX) } else { (0, 0) };
    }
    let (t0, t1) = ((lo - s0) / step, (hi - s0) / step);
    ((t0.min(t1).floor() as i64).saturating_sub(1), (t0.max(t1).ceil() as i64).saturating_add(2))
}

/// 四个点的外接范围：(最小 x, 最大 x, 最小 y, 最大 y)。
fn extent(p: [(f64, f64); 4]) -> (f64, f64, f64, f64) {
    let lo = |a: f64, b: f64| a.min(b);
    let hi = |a: f64, b: f64| a.max(b);
    (
        p.iter().map(|q| q.0).fold(f64::INFINITY, lo),
        p.iter().map(|q| q.0).fold(f64::NEG_INFINITY, hi),
        p.iter().map(|q| q.1).fold(f64::INFINITY, lo),
        p.iter().map(|q| q.1).fold(f64::NEG_INFINITY, hi),
    )
}

/// 一个 64 位整数里并排放两个通道，各用 32 位里的低 16 位；空着的高位留给乘权重时的进位。
const PAIR: u64 = 0x0000_FFFF_0000_FFFF;

/// 两个通道一起在 a、b 之间按 t（0..ONE）线性插值，四舍五入。
/// 每个通道的中间结果不超过 2³¹，不会串到隔壁；结果不会跑到 a、b 之外。
#[inline]
fn lerp2(a: u64, b: u64, t: u64) -> u64 {
    let half = (ONE as u64 / 2) * 0x1_0000_0001;
    ((a * (ONE as u64 - t) + b * t + half) >> 15) & PAIR
}

/// Catmull-Rom 核在相邻四个采样点上的权重，t（0..1）是采样位置到第二个点的距离。四个权重之和恒为 1。
#[inline]
fn catmull_rom(t: f32) -> [f32; 4] {
    let (t2, t3) = (t * t, t * t * t);
    [-0.5 * t3 + t2 - 0.5 * t, 1.5 * t3 - 2.5 * t2 + 1.0, -1.5 * t3 + 2.0 * t2 + 0.5 * t, 0.5 * t3 - 0.5 * t2]
}

/// 把累加出来的浮点值收成一个合法的预乘像素。三次核有负的旁瓣，硬边旁边会过冲，
/// 这里压回 0 ≤ 颜色 ≤ 透明度 ≤ ONE。
#[inline]
fn finish(acc: [f32; 4], norm: f32) -> [u16; 4] {
    let round = |v: f32, max: f32| (v * norm + 0.5).clamp(0.0, max) as u16;
    let a = round(acc[3], ONE as f32);
    [round(acc[0], a as f32), round(acc[1], a as f32), round(acc[2], a as f32), a]
}

#[inline]
fn rgba(s: &[u16], o: usize) -> [u16; 4] {
    let p = &s[o..o + 4];
    [p[0], p[1], p[2], p[3]]
}

#[inline]
fn mul15(a: u32, b: u32) -> u32 {
    (a * b + ONE / 2) >> 15
}

/// 分块的像素数据。`Tile` 里的 Vec 是公开的，长度不对的（不该有）一律当作没有这个分块。
fn pixels(t: &Tile) -> Option<&[u16; LEN]> {
    t.0.as_slice().try_into().ok()
}

/// 新算出来的分块只在 `clip`（画布）里算数：分块伸出 `clip` 的那部分换回原来的像素。
/// 液化、滤镜只改画布里的，画布外的内容原样不动（实现方案 §3.15）。
pub fn keep_outside(new: Option<Arc<Tile>>, old: Option<&Arc<Tile>>, tc: TileCoord, clip: Rect) -> Option<Arc<Tile>> {
    let inside = tile_rect_in(tc, clip);
    let whole = Rect { x0: tc.0 * TILE, y0: tc.1 * TILE, x1: (tc.0 + 1) * TILE, y1: (tc.1 + 1) * TILE };
    // 长度不对的分块当作不存在；原样没动的分块照旧共用。
    let Some(old) = old.filter(|o| o.0.len() == N * 4) else { return new };
    if inside == whole || new.as_ref().is_some_and(|n| Arc::ptr_eq(n, old)) {
        return new;
    }
    if inside.is_empty() {
        return Some(old.clone());
    }
    let outside = |x: i32, y: i32| x < inside.x0 || x >= inside.x1 || y < inside.y0 || y >= inside.y1;
    let index = |x: i32, y: i32| (((y - whole.y0) * TILE + (x - whole.x0)) * 4) as usize;
    let pixels = || (whole.y0..whole.y1).flat_map(move |y| (whole.x0..whole.x1).map(move |x| (x, y))).filter(|&(x, y)| outside(x, y));
    // 原来的分块伸出去的那部分是空的，就没有要留的。
    if pixels().all(|(x, y)| old.0[index(x, y) + 3] == 0) {
        return new;
    }
    let mut t = new.as_deref().filter(|n| n.0.len() == N * 4).cloned().unwrap_or_else(Tile::new);
    for (x, y) in pixels() {
        let k = index(x, y);
        t.0[k..k + 4].copy_from_slice(&old.0[k..k + 4]);
    }
    visible(&t.0).then(|| Arc::new(t))
}

fn visible(data: &[u16]) -> bool {
    data.as_chunks::<4>().0.iter().any(|p| p[3] != 0)
}

/// 分块 tc 落在 r 里的那部分。用 64 位算，坐标再离谱也不会溢出。
fn tile_rect_in(tc: TileCoord, r: Rect) -> Rect {
    let (ox, oy) = (tc.0 as i64 * TILE as i64, tc.1 as i64 * TILE as i64);
    let (x0, y0) = (ox.max(r.x0 as i64), oy.max(r.y0 as i64));
    let (x1, y1) = ((ox + TILE as i64).min(r.x1 as i64), (oy + TILE as i64).min(r.y1 as i64));
    if x1 <= x0 || y1 <= y0 {
        Rect::EMPTY
    } else {
        Rect { x0: x0 as i32, y0: y0 as i32, x1: x1 as i32, y1: y1 as i32 }
    }
}

/// 分块 tc 的数据里，第 y 行上 r 的横向范围所占的下标区间。r 必须在这个分块之内。
fn row_range(tc: TileCoord, r: Rect, y: i32) -> std::ops::Range<usize> {
    let (ox, oy) = (tc.0 as i64 * TILE as i64, tc.1 as i64 * TILE as i64);
    let o = ((y as i64 - oy) * TILE as i64 + (r.x0 as i64 - ox)) as usize * 4;
    o..o + (r.x1 - r.x0) as usize * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, PI};
    use std::time::Instant;

    const H: u16 = (ONE / 2) as u16;

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

    /// 伪随机的预乘像素：不透明、半透明、全透明的都有。
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

    /// 逐像素对照：`area` 里每个像素都要和 `want` 给出的一模一样。
    fn assert_pixels(got: &TileMap, area: Rect, want: impl Fn(i32, i32) -> [u16; 4]) {
        for (x, y) in pixels_in(area) {
            assert_eq!(at(got, x, y), want(x, y), "({x}, {y})");
        }
    }

    /// 没有选区时结果必须满足的几条：不留全透明的分块，clip 外面什么都没有，像素是合法的预乘值。
    fn assert_tidy(out: &TileMap, clip: Rect) {
        for tc in out.coords() {
            let t = out.get(tc).unwrap();
            assert!(visible(&t.0), "分块 {tc:?} 全透明");
            for (i, p) in t.0.as_chunks::<4>().0.iter().enumerate() {
                let (x, y) = (tc.0 * TILE + i as i32 % TILE, tc.1 * TILE + i as i32 / TILE);
                let inside = x >= clip.x0 && x < clip.x1 && y >= clip.y0 && y < clip.y1;
                assert!(inside || *p == [0; 4], "({x}, {y}) 在 clip 之外却有内容");
                assert!(p[3] as u32 <= ONE && p[..3].iter().all(|&c| c <= p[3]), "({x}, {y}) 不是合法的预乘像素：{p:?}");
            }
        }
    }

    fn alpha_sum(tiles: &TileMap, area: Rect) -> f64 {
        pixels_in(area).map(|(x, y)| at(tiles, x, y)[3] as f64).sum()
    }

    /// 两张图在 `area` 里各通道差的绝对值之和，以"多少个完全不同的像素"计。
    fn difference(a: &TileMap, b: &TileMap, area: Rect) -> f64 {
        let per_px = |(x, y)| {
            let (p, q) = (at(a, x, y), at(b, x, y));
            (0..4).map(|c| p[c].abs_diff(q[c]) as f64).sum::<f64>()
        };
        pixels_in(area).map(per_px).sum::<f64>() / (4.0 * ONE as f64)
    }

    fn close(p: (f32, f32), q: (f32, f32)) -> bool {
        (p.0 - q.0).abs() < 1e-4 && (p.1 - q.1).abs() < 1e-4
    }

    #[test]
    fn affine_composes_and_inverts() {
        let t = Affine::translate(3.0, -2.0);
        let s = Affine::scale_about(2.0, 3.0, 0.0, 0.0);
        // 先平移再缩放，和先缩放再平移不是一回事。
        assert_eq!(t.then(&s).apply(1.0, 1.0), (8.0, -3.0));
        assert_eq!(s.then(&t).apply(1.0, 1.0), (5.0, 1.0));
        assert_eq!(Affine::IDENTITY.then(&t), t);
        assert_eq!(t.then(&Affine::IDENTITY), t);

        let m = Affine::rotate_about(0.7, 30.0, -12.0).then(&Affine::scale_about(1.5, 0.6, 5.0, 9.0)).then(&t);
        let inv = m.inverse().unwrap();
        for p in [(0.0, 0.0), (17.5, -40.25), (64.0, 100.0)] {
            let q = m.apply(p.0, p.1);
            assert!(close(inv.apply(q.0, q.1), p), "{p:?}");
        }
        let both = m.then(&inv);
        for (got, want) in [both.a, both.b, both.c, both.d, both.e, both.f].into_iter().zip([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]) {
            assert!((got - want).abs() < 1e-4, "{both:?}");
        }

        assert!(Affine::scale_about(0.0, 1.0, 5.0, 5.0).inverse().is_none());
        assert!(Affine { a: 1.0, b: 2.0, c: 2.0, d: 4.0, e: 0.0, f: 0.0 }.inverse().is_none());
        assert!(Affine { a: f32::NAN, ..Affine::IDENTITY }.inverse().is_none());
        assert!(Affine { e: f32::INFINITY, ..Affine::IDENTITY }.inverse().is_none());
    }

    #[test]
    fn rotation_and_scaling_keep_their_centre() {
        let r = Affine::rotate_about(1.1, 40.5, 77.25);
        assert!(close(r.apply(40.5, 77.25), (40.5, 77.25)));
        let s = Affine::scale_about(3.0, -0.25, 40.5, 77.25);
        assert!(close(s.apply(40.5, 77.25), (40.5, 77.25)));
        assert!(close(s.apply(41.5, 81.25), (43.5, 76.25)));
        // 正角度在屏幕上是顺时针：中心正右方的点转到正下方。
        let q = Affine::rotate_about(FRAC_PI_2, 10.0, 10.0);
        assert!(close(q.apply(11.0, 10.0), (10.0, 11.0)));
        // 直角的整数倍得到的是精确的矩阵。
        assert_eq!(q, Affine { a: 0.0, b: 1.0, c: -1.0, d: 0.0, e: 20.0, f: 0.0 });
        assert_eq!(Affine::rotate_about(PI, 8.0, 3.0), Affine { a: -1.0, b: 0.0, c: 0.0, d: -1.0, e: 16.0, f: 6.0 });
    }

    #[test]
    fn identity_and_whole_pixel_moves_are_exact() {
        let clip = rect(0, 0, 300, 200);
        // 内容跨着好几个分块的边界。
        let area = rect(50, 30, 150, 140);
        let src = noise_in(area);
        for quality in [Quality::Fast, Quality::Best] {
            let same = transform_tiles(&src, None, &Affine::IDENTITY, quality, clip);
            assert_pixels(&same, clip, |x, y| at(&src, x, y));

            let moved = transform_tiles(&src, None, &Affine::translate(37.0, -19.0), quality, clip);
            assert_pixels(&moved, clip, |x, y| at(&src, x - 37, y + 19));
            assert_tidy(&moved, clip);

            // 平移量恰好是分块边长时也一样。
            let moved = transform_tiles(&src, None, &Affine::translate(64.0, 0.0), quality, clip);
            assert_pixels(&moved, clip, |x, y| at(&src, x - 64, y));
        }
        // 离整数差得很少的平移量按整数算，不会因此糊掉。
        let nearly = transform_tiles(&src, None, &Affine::translate(5.0004, -2.9996), Quality::Fast, clip);
        assert_pixels(&nearly, clip, |x, y| at(&src, x - 5, y + 3));
    }

    #[test]
    fn flips_and_quarter_turns_are_exact() {
        // 方形画布，内容铺满，四条边上都有像素。
        let n = 200;
        let clip = rect(0, 0, n, n);
        let src = noise_in(clip);
        let c = n as f32 / 2.0;
        for quality in [Quality::Fast, Quality::Best] {
            let flip_h = transform_tiles(&src, None, &Affine::scale_about(-1.0, 1.0, c, c), quality, clip);
            assert_pixels(&flip_h, clip, |x, y| at(&src, n - 1 - x, y));
            let flip_v = transform_tiles(&src, None, &Affine::scale_about(1.0, -1.0, c, c), quality, clip);
            assert_pixels(&flip_v, clip, |x, y| at(&src, x, n - 1 - y));

            // 顺时针 90°：原来的左上角到了右上角。
            let turn = Affine::rotate_about(FRAC_PI_2, c, c);
            let r90 = transform_tiles(&src, None, &turn, quality, clip);
            assert_pixels(&r90, clip, |x, y| at(&src, y, n - 1 - x));
            let r180 = transform_tiles(&src, None, &Affine::rotate_about(PI, c, c), quality, clip);
            assert_pixels(&r180, clip, |x, y| at(&src, n - 1 - x, n - 1 - y));
            let r270 = transform_tiles(&src, None, &Affine::rotate_about(3.0 * FRAC_PI_2, c, c), quality, clip);
            assert_pixels(&r270, clip, |x, y| at(&src, n - 1 - y, x));
            assert_pixels(&transform_tiles(&src, None, &Affine::rotate_about(-FRAC_PI_2, c, c), quality, clip), clip, |x, y| at(&r270, x, y));

            let mut back = r90;
            for _ in 0..3 {
                back = transform_tiles(&back, None, &turn, quality, clip);
            }
            assert_pixels(&back, clip, |x, y| at(&src, x, y));
            // 由 then 拼出来的矩阵也认得出来。
            let twice = transform_tiles(&src, None, &turn.then(&turn), quality, clip);
            assert_pixels(&twice, clip, |x, y| at(&r180, x, y));
        }
    }

    #[test]
    fn quarter_turn_on_an_oblong_canvas() {
        // 宽高不等但都是偶数：绕画布中心转 90° 仍然落在像素格上，只是转出画布的部分被裁掉。
        let clip = rect(0, 0, 200, 120);
        let src = noise_in(rect(40, 0, 160, 120));
        let turn = Affine::rotate_about(FRAC_PI_2, 100.0, 60.0);
        let r90 = transform_tiles(&src, None, &turn, Quality::Best, clip);
        // 像素 (i, j) 转到 (159 − j, i − 40)。
        assert_pixels(&r90, clip, |x, y| if (40..160).contains(&x) { at(&src, y + 40, 159 - x) } else { [0; 4] });
        let mut back = r90;
        for _ in 0..3 {
            back = transform_tiles(&back, None, &turn, Quality::Best, clip);
        }
        assert_pixels(&back, clip, |x, y| at(&src, x, y));

        let whole = noise_in(clip);
        let cut = transform_tiles(&whole, None, &turn, Quality::Best, clip);
        assert_tidy(&cut, clip);
        assert_pixels(&cut, clip, |x, y| if (40..160).contains(&x) { at(&whole, y + 40, 159 - x) } else { [0; 4] });
    }

    #[test]
    fn moving_off_the_canvas_clips() {
        let clip = rect(0, 0, 200, 150);
        let src = noise_in(rect(120, 90, 200, 150));
        for (dx, dy) in [(50.0, 40.0), (50.5, 40.25)] {
            for quality in [Quality::Fast, Quality::Best] {
                let out = transform_tiles(&src, None, &Affine::translate(dx, dy), quality, clip);
                assert_tidy(&out, clip);
                assert!(!out.is_empty());
                // 内容的左上角移到了 (170, 130) 附近；留出三次核够得着的两个像素。
                assert!(pixels_in(clip).all(|(x, y)| (x >= 168 && y >= 128) || at(&out, x, y) == [0; 4]));
            }
        }
        let out = transform_tiles(&src, None, &Affine::translate(50.0, 40.0), Quality::Best, clip);
        assert_pixels(&out, clip, |x, y| at(&src, x - 50, y - 40));
        // 整个移出画布：什么都不剩。
        assert!(transform_tiles(&src, None, &Affine::translate(500.0, 0.0), Quality::Fast, clip).is_empty());
        assert!(transform_tiles(&src, None, &Affine::translate(500.5, 0.0), Quality::Best, clip).is_empty());

        // 边缘分块里画布之外的像素不算数：往回移也移不进画布。
        let spill = noise_in(rect(150, 100, 256, 192));
        let inside = transform_tiles(&spill, None, &Affine::translate(-40.0, -30.0), Quality::Best, clip);
        assert_tidy(&inside, clip);
        assert_pixels(&inside, clip, |x, y| if x + 40 < 200 && y + 30 < 150 { at(&spill, x + 40, y + 30) } else { [0; 4] });
        let blurred = transform_tiles(&spill, None, &Affine::translate(-40.5, -30.5), Quality::Fast, clip);
        assert_tidy(&blurred, clip);
        assert!(pixels_in(rect(160, 0, 200, 150)).all(|(x, y)| at(&blurred, x, y) == [0; 4]));
        // 恒等变换的结果同样只有画布里的部分。
        let same = transform_tiles(&spill, None, &Affine::IDENTITY, Quality::Fast, clip);
        assert_tidy(&same, clip);
        assert_pixels(&same, clip, |x, y| at(&spill, x, y));
        // 有选区时，画布外面的像素不算选中，和其他没选中的像素一样原样留下。
        let sel = Selection::rect(rect(150, 100, 300, 300));
        let kept = transform_tiles(&spill, Some(&sel), &Affine::translate(-40.0, -30.0), Quality::Best, clip);
        assert_eq!(at(&kept, 230, 170), at(&spill, 230, 170));
        assert_eq!(at(&kept, 120, 80), at(&spill, 160, 110));
        assert_eq!(at(&kept, 199, 149), [0; 4]);
        assert!(transform_tiles(&TileMap::default(), None, &Affine::translate(1.5, 0.0), Quality::Best, clip).is_empty());
    }

    #[test]
    fn doubling_then_halving_comes_back() {
        let clip = rect(0, 0, 256, 256);
        let area = rect(100, 110, 150, 150);
        let mut src = TileMap::default();
        // 一块不透明的色块，右边一条半透明的。
        paint(&mut src, area, |x, _| if x < 135 { [ONE as u16, H, 0, ONE as u16] } else { [H / 2, H, H, H] });
        let (up, down) = (Affine::scale_about(2.0, 2.0, 128.0, 128.0), Affine::scale_about(0.5, 0.5, 128.0, 128.0));
        for quality in [Quality::Fast, Quality::Best] {
            let big = transform_tiles(&src, None, &up, quality, clip);
            assert_tidy(&big, clip);
            // 放大两倍，覆盖的面积是四倍。
            let ratio = alpha_sum(&big, clip) / alpha_sum(&src, clip);
            assert!((ratio - 4.0).abs() < 0.02, "{quality:?}: {ratio}");
            // 色块内部仍是原来的颜色，一点不差。
            assert_eq!(at(&big, 100, 128), [ONE as u16, H, 0, ONE as u16]);
            assert_eq!(at(&big, 160, 128), [H / 2, H, H, H]);

            let back = transform_tiles(&big, None, &down, quality, clip);
            assert_tidy(&back, clip);
            let err = difference(&back, &src, clip) / (50.0 * 40.0);
            assert!(err < 0.02, "{quality:?}: 平均误差 {err}");
        }
    }

    #[test]
    fn rotating_there_and_back_comes_back() {
        let clip = rect(0, 0, 256, 256);
        let area = rect(83, 98, 173, 158);
        let mut src = TileMap::default();
        paint(&mut src, area, |_, _| [H, ONE as u16, H / 4, ONE as u16]);
        let angle = 30f32.to_radians();
        let there = Affine::rotate_about(angle, 128.0, 128.0);
        let back = Affine::rotate_about(-angle, 128.0, 128.0);
        let mut errors = Vec::new();
        for (quality, limit) in [(Quality::Best, 0.012), (Quality::Fast, 0.02)] {
            let turned = transform_tiles(&src, None, &there, quality, clip);
            assert_tidy(&turned, clip);
            // 转过去之后面积不变，四个角确实转开了。
            let ratio = alpha_sum(&turned, clip) / alpha_sum(&src, clip);
            assert!((ratio - 1.0).abs() < 0.01, "{quality:?}: {ratio}");
            assert_eq!(at(&turned, 84, 99)[3], 0);
            assert_eq!(at(&turned, 128, 128), [H, ONE as u16, H / 4, ONE as u16]);

            let restored = transform_tiles(&turned, None, &back, quality, clip);
            assert_tidy(&restored, clip);
            let err = difference(&restored, &src, clip) / (90.0 * 60.0);
            assert!(err < limit, "{quality:?}: 平均误差 {err}");
            errors.push(err);
        }
        // 三次核比双线性锐，来回转一趟丢的东西更少。
        assert!(errors[0] < errors[1], "{errors:?}");
        // 带噪声的半透明内容转过去也仍是合法的预乘像素（三次核会过冲）。
        let noisy = noise_in(area);
        for quality in [Quality::Best, Quality::Fast] {
            assert_tidy(&transform_tiles(&noisy, None, &there, quality, clip), clip);
            assert_tidy(&transform_tiles(&noisy, None, &there.then(&Affine::scale_about(0.37, 1.9, 128.0, 128.0)), quality, clip), clip);
        }
    }

    #[test]
    fn thin_lines_survive_halving() {
        let clip = rect(0, 0, 256, 256);
        // 线上第 i 个像素的位置。
        type Line = fn(i32) -> (i32, i32);
        let lines: [(&str, Line); 3] = [("横线", |i| (40 + i, 101)), ("竖线", |i| (78, 40 + i)), ("斜线", |i| (40 + i, 50 + i))];
        for (name, line) in lines {
            // 两种相位：缩小后线正好落在像素中间，或者落在两个像素之间。
            //（后一种相位下用 `Fast` 缩，横线会整条消失：它只在目标像素的中心采一个点。）
            for centre in [128.0, 128.5] {
                let mut src = TileMap::default();
                for i in 0..160 {
                    let (x, y) = line(i);
                    paint(&mut src, rect(x, y, x + 1, y + 1), |_, _| [ONE as u16; 4]);
                }
                let m = Affine::scale_about(0.5, 0.5, centre, centre);
                let out = transform_tiles(&src, None, &m, Quality::Best, clip);
                assert_tidy(&out, clip);
                // 沿着线走，每一处横穿过线的覆盖率之和都应该在一半上下：不断，也不忽粗忽细。
                for i in 8..152 {
                    let (x, y) = line(i);
                    let (u, v) = m.apply(x as f32 + 0.5, y as f32 + 0.5);
                    let (u, v) = (u.floor() as i32, v.floor() as i32);
                    let across: u32 = (-3..=3).map(|k| if name == "竖线" { at(&out, u + k, v) } else { at(&out, u, v + k) }[3] as u32).sum();
                    let cover = across as f32 / ONE as f32;
                    assert!((0.45..0.6).contains(&cover), "{name}，中心 {centre}，第 {i} 点：覆盖率 {cover}");
                }
                let ratio = alpha_sum(&out, clip) / alpha_sum(&src, clip);
                assert!((ratio - 0.25).abs() < 0.04, "{name}，中心 {centre}：总覆盖率之比 {ratio}");
            }
        }
    }

    #[test]
    fn only_the_selected_part_moves() {
        let clip = rect(0, 0, 200, 150);
        let src = noise_in(rect(20, 20, 120, 60));
        let sel = Selection::rect(rect(70, 0, 200, 150));
        let m = Affine::translate(0.0, 50.0);
        let out = transform_tiles(&src, Some(&sel), &m, Quality::Best, clip);
        // 左半边没选中，原样不动；右半边整个下移了 50，原来的位置空了。
        assert_pixels(&out, clip, |x, y| if x < 70 { at(&src, x, y) } else { at(&src, x, y - 50) });
        // 选区和落点都碰不到的分块连复制都没有发生。
        assert!(Arc::ptr_eq(out.get((0, 0)).unwrap(), src.get((0, 0)).unwrap()));

        let moved = transform_selection(&sel, &m, clip);
        assert_eq!(moved.bounds(), rect(70, 50, 200, 150));
        for (x, y) in pixels_in(clip) {
            assert_eq!(moved.get(x, y) as u32, if x >= 70 && y >= 50 { ONE } else { 0 }, "({x}, {y})");
        }
        // 内容落到哪里，选区就跟到哪里。
        assert!(pixels_in(clip).all(|(x, y)| x < 70 || at(&out, x, y)[3] == 0 || moved.get(x, y) as u32 == ONE));

        // 非整数的变换也一样：没选中的那半边一个比特都不变。
        let turn = Affine::rotate_about(0.4, 95.0, 40.0).then(&Affine::translate(30.5, 55.0));
        for quality in [Quality::Fast, Quality::Best] {
            let out = transform_tiles(&src, Some(&sel), &turn, quality, clip);
            assert_pixels(&out, rect(0, 0, 70, 70), |x, y| at(&src, x, y));
            assert!(Arc::ptr_eq(out.get((0, 0)).unwrap(), src.get((0, 0)).unwrap()));
            assert!(pixels_in(rect(70, 0, 200, 62)).all(|(x, y)| at(&out, x, y)[3] == 0), "选中的部分已经离开原位");
            assert!(alpha_sum(&out, rect(70, 62, 200, 150)) > 0.0);
        }
        let turned = transform_selection(&sel, &turn, clip);
        let b = turned.bounds();
        assert!(!b.is_empty() && b.x0 >= 0 && b.y0 >= 0 && b.x1 <= 200 && b.y1 <= 150, "{b:?}");
        // 选区的一个内点、一个外点，按同一个矩阵算过去。
        let (ix, iy) = turn.apply(110.5, 40.5);
        assert_eq!(turned.get(ix as i32, iy as i32) as u32, ONE);
        let (ox, oy) = turn.apply(60.5, 40.5);
        assert_eq!(turned.get(ox as i32, oy as i32), 0);

        // 恒等变换、空选区、选区里没有内容：都等于什么也没做。
        for (s, m) in [(&sel, Affine::IDENTITY), (&Selection::empty(), m), (&Selection::rect(rect(130, 70, 190, 140)), m)] {
            assert_pixels(&transform_tiles(&src, Some(s), &m, Quality::Best, clip), clip, |x, y| at(&src, x, y));
        }
        assert!(transform_selection(&Selection::empty(), &m, clip).is_empty());
        let same = transform_selection(&sel, &Affine::IDENTITY, clip);
        assert_eq!(same.bounds(), rect(70, 0, 200, 150));
    }

    #[test]
    fn partial_selection_coverage_is_honoured() {
        let clip = rect(0, 0, 200, 100);
        let colour = [ONE as u16, H, 100, ONE as u16];
        let mut src = TileMap::default();
        paint(&mut src, rect(10, 10, 100, 50), |_, _| colour);
        // 选区在 10..40 这一段只选中一半。
        let mut mask = MaskMap::default();
        for (x, y) in pixels_in(rect(10, 0, 40, 100)) {
            mask.get_mut((x / TILE, y / TILE))[(y % TILE * TILE + x % TILE) as usize] = H;
        }
        let sel = Selection::from_mask(mask, rect(10, 0, 40, 100));
        let half = [H, H / 2, 50, H];
        let out = transform_tiles(&src, Some(&sel), &Affine::translate(50.0, 30.0), Quality::Best, clip);
        // 原位留下一半。
        assert_eq!(at(&out, 20, 20), half);
        // 没选中、也没被盖住的地方不变。
        assert_eq!(at(&out, 50, 20), colour);
        assert_eq!(at(&out, 95, 45), colour);
        // 移过去的一半落在空白处。
        assert_eq!(at(&out, 70, 60), half);
        // 移过去的一半盖在不透明的内容上：仍然不透明，颜色是两者各半（同色，所以不变）。
        let p = at(&out, 70, 45);
        assert_eq!(p[3] as u32, ONE);
        assert!(p.iter().zip(colour).all(|(&a, b)| a.abs_diff(b) <= 1), "{p:?}");
        // 选区本身也按一半的覆盖率移过去。
        let moved = transform_selection(&sel, &Affine::translate(50.0, 30.0), clip);
        assert_eq!((moved.get(70, 60), moved.get(70, 20), moved.get(95, 60)), (H, 0, 0));
        assert_eq!(moved.bounds(), rect(60, 30, 90, 100));
    }

    /// 对照用的笨办法：一个目标像素按定义从头算一遍。核函数直接照距离写，逐个采样点查哈希，
    /// 全程双精度，不分块、不跳行、不用定点数。
    fn reference(src: &TileMap, m: &Affine, quality: Quality, clip: Rect, x: i32, y: i32) -> [f64; 4] {
        let inv = Mat64::of(m).inverse().unwrap();
        let (radius, nx, ny) = match quality {
            Quality::Fast => (1, 1, 1),
            Quality::Best => (2, subsamples(inv.a.hypot(inv.b)), subsamples(inv.c.hypot(inv.d))),
        };
        let kernel = |d: f64| match (quality, d.abs()) {
            (Quality::Fast, d) => (1.0 - d).max(0.0),
            (Quality::Best, d) if d <= 1.0 => 1.5 * d * d * d - 2.5 * d * d + 1.0,
            (Quality::Best, d) if d < 2.0 => -0.5 * d * d * d + 2.5 * d * d - 4.0 * d + 2.0,
            _ => 0.0,
        };
        let mut acc = [0f64; 4];
        for (i, j) in pixels_in(rect(0, 0, nx, ny)) {
            let (u, v) = inv.apply(x as f64 + (i as f64 + 0.5) / nx as f64, y as f64 + (j as f64 + 0.5) / ny as f64);
            let (u, v) = (u - 0.5, v - 0.5);
            for (px, py) in pixels_in(rect(u.floor() as i32 - radius + 1, v.floor() as i32 - radius + 1, u.floor() as i32 + radius + 1, v.floor() as i32 + radius + 1)) {
                let inside = px >= clip.x0 && px < clip.x1 && py >= clip.y0 && py < clip.y1;
                let p = if inside { at(src, px, py) } else { [0; 4] };
                let w = kernel(u - px as f64) * kernel(v - py as f64) / (nx * ny) as f64;
                for (a, c) in acc.iter_mut().zip(p) {
                    *a += w * c as f64;
                }
            }
        }
        let a = acc[3].clamp(0.0, ONE as f64);
        [acc[0].clamp(0.0, a), acc[1].clamp(0.0, a), acc[2].clamp(0.0, a), a]
    }

    #[test]
    fn resampling_matches_the_definition() {
        let clip = rect(0, 0, 120, 100);
        // 内容跨着分块边界，右边一截伸到画布外面（应当被当作透明）。
        let src = noise_in(rect(20, 15, 140, 85));
        let about = |m: Affine| m.then(&Affine::translate(0.25, -0.5));
        let cases = [
            about(Affine::rotate_about(0.3, 60.0, 50.0).then(&Affine::scale_about(1.3, 0.8, 60.0, 50.0))),
            about(Affine::scale_about(0.45, 0.45, 60.0, 50.0).then(&Affine::rotate_about(-1.0, 60.0, 50.0))),
            about(Affine::scale_about(-1.0, 2.5, 60.0, 50.0)),
            Affine { a: 1.0, b: 0.2, c: -0.35, d: 0.9, e: 12.3, f: -7.7 },
            Affine::translate(0.5, 0.25),
        ];
        for m in cases {
            for quality in [Quality::Fast, Quality::Best] {
                let out = transform_tiles(&src, None, &m, quality, clip);
                assert_tidy(&out, clip);
                for (x, y) in pixels_in(clip) {
                    let (got, want) = (at(&out, x, y), reference(&src, &m, quality, clip, x, y));
                    // 定点的坐标和权重各有不到一个单位的舍入，留几个单位的余量。
                    assert!(got.iter().zip(want).all(|(&g, w)| (g as f64 - w).abs() <= 3.0), "{m:?} {quality:?} ({x}, {y}): {got:?} 对 {want:?}");
                }
            }
        }
    }

    #[test]
    fn content_bounds_are_tight_and_clipped() {
        let clip = rect(0, 0, 300, 200);
        assert_eq!(content_bounds(&TileMap::default(), clip), Rect::EMPTY);
        let mut t = TileMap::default();
        // 只有颜色没有透明度的像素不算内容；全透明的分块也不算。
        paint(&mut t, rect(5, 5, 6, 6), |_, _| [9, 9, 9, 0]);
        t.get_mut((3, 1));
        assert_eq!(content_bounds(&t, clip), Rect::EMPTY);
        paint(&mut t, rect(70, 40, 71, 41), |_, _| [0, 0, 0, 1]);
        assert_eq!(content_bounds(&t, clip), rect(70, 40, 71, 41));
        paint(&mut t, rect(130, 63, 200, 65), |_, _| [0, 0, 0, ONE as u16]);
        assert_eq!(content_bounds(&t, clip), rect(70, 40, 200, 65));
        paint(&mut t, rect(-10, 100, 3, 130), |_, _| [0, 0, 0, ONE as u16]);
        paint(&mut t, rect(290, 190, 320, 230), |_, _| [0, 0, 0, ONE as u16]);
        assert_eq!(content_bounds(&t, clip), rect(0, 40, 300, 200));
        assert_eq!(content_bounds(&t, rect(100, 0, 150, 64)), rect(130, 63, 150, 64));
        assert_eq!(content_bounds(&t, rect(72, 0, 125, 300)), Rect::EMPTY);
        assert_eq!(content_bounds(&t, Rect::EMPTY), Rect::EMPTY);
        let everything = rect(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        assert_eq!(content_bounds(&t, everything), rect(-10, 40, 320, 230));
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        let clip = rect(0, 0, 200, 150);
        let src = noise_in(rect(20, 20, 120, 90));
        let sel = Selection::rect(rect(40, 30, 90, 70));
        let bad = [
            Affine::scale_about(0.0, 1.0, 50.0, 50.0),
            Affine { a: 1.0, b: 1.0, c: 1.0, d: 1.0, e: 0.0, f: 0.0 },
            Affine { a: 0.0, b: 0.0, c: 0.0, d: 0.0, e: 0.0, f: 0.0 },
            Affine { c: f32::NAN, ..Affine::IDENTITY },
            Affine { f: f32::NAN, ..Affine::IDENTITY },
            Affine { a: f32::INFINITY, ..Affine::IDENTITY },
            Affine { e: f32::NEG_INFINITY, ..Affine::IDENTITY },
        ];
        for m in bad {
            for s in [None, Some(&sel)] {
                // 没法变换的矩阵：原样返回，连分块都是共用的。
                let out = transform_tiles(&src, s, &m, Quality::Best, clip);
                assert_eq!(out.len(), src.len(), "{m:?}");
                assert!(src.coords().all(|tc| Arc::ptr_eq(out.get(tc).unwrap(), src.get(tc).unwrap())), "{m:?}");
            }
            assert!(transform_selection(&sel, &m, clip).is_empty(), "{m:?}");
        }
        // 能求逆但大小离谱的矩阵：结果是什么不要紧，不能崩，也不能越界。
        let odd = [
            Affine::scale_about(1e-6, 1e-6, 70.0, 55.0),
            Affine::scale_about(1e-20, 1.0, 70.0, 55.0),
            Affine::scale_about(1e6, 1e6, 70.0, 55.0),
            Affine::scale_about(3e4, 1e-5, 70.0, 55.0),
            Affine::scale_about(-1e-3, 250.0, 70.0, 55.0),
            Affine::translate(1e30, -1e30),
            Affine::translate(3e9, 0.25),
            Affine { a: 1.0, b: 1e15, c: 0.0, d: 1.0, e: 0.0, f: 0.0 },
            Affine { a: 1e-30, b: 0.0, c: 0.0, d: 1e30, e: 1e20, f: -1e20 },
            Affine { a: 1e-40, b: 0.0, c: 0.0, d: 1e-40, e: 0.0, f: 0.0 },
            Affine::rotate_about(0.3, 1e7, -1e7),
        ];
        for m in odd {
            for quality in [Quality::Fast, Quality::Best] {
                assert_tidy(&transform_tiles(&src, None, &m, quality, clip), clip);
                transform_tiles(&src, Some(&sel), &m, quality, clip);
            }
            transform_selection(&sel, &m, clip);
        }
        // clip 是空的、或者和内容不沾边。
        for c in [Rect::EMPTY, rect(500, 500, 600, 600), rect(10, 10, 5, 5)] {
            assert!(transform_tiles(&src, None, &Affine::translate(0.5, 0.5), Quality::Best, c).is_empty());
            assert!(transform_tiles(&src, None, &Affine::IDENTITY, Quality::Best, c).is_empty());
            assert!(transform_selection(&sel, &Affine::translate(3.0, 0.5), c).is_empty());
        }
        // 长度不对的分块当作不存在。
        let mut broken = src.clone();
        broken.replace((0, 0), Some(Arc::new(Tile(vec![ONE as u16; 7]))));
        assert_tidy(&transform_tiles(&broken, None, &Affine::translate(0.5, 0.0), Quality::Best, clip), clip);
        transform_tiles(&broken, Some(&sel), &Affine::translate(2.0, 0.0), Quality::Fast, clip);
        content_bounds(&broken, clip);
    }

    /// 性能测试用的画面：2000×1500 的内容，带渐变，相邻像素各不相同；画布比内容大一圈。
    fn big_picture() -> (TileMap, Rect) {
        let mut src = TileMap::default();
        paint(&mut src, rect(400, 400, 2400, 1900), |x, y| {
            let a = (ONE / 4 + ((x * 7 + y * 13) as u32 % (ONE * 3 / 4))) as u16;
            [a / 2, (x as u32 * 16 % a as u32) as u16, a, a]
        });
        (src, rect(0, 0, 2800, 2300))
    }

    /// 变换一次的毫秒数。取五次里最快的，免得被同时在跑的别的任务拖累。
    fn millis(src: &TileMap, m: &Affine, quality: Quality, clip: Rect) -> f64 {
        let once = |_| {
            let t = Instant::now();
            let out = transform_tiles(src, None, m, quality, clip);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(!out.is_empty());
            ms
        };
        (0..5).map(once).fold(f64::MAX, f64::min)
    }

    /// 2000×1500 的内容用 `Fast` 变换一次要远低于 100 毫秒：拖动时每次鼠标移动都要跑一遍。
    /// 工作区的调试构建也开着优化，所以这条不分构建方式。
    #[test]
    fn fast_preview_is_fast_enough() {
        let (src, clip) = big_picture();
        let m = Affine::rotate_about(0.2, 1400.0, 1150.0).then(&Affine::scale_about(1.07, 1.07, 1400.0, 1150.0));
        let ms = millis(&src, &m, Quality::Fast, clip);
        assert!(ms < 100.0, "Fast 用了 {ms:.1} ms");
    }

    /// 各种情况下的耗时，手动跑着看：
    /// `cargo test -p zhibi-core --release transform::tests::timings -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn timings() {
        let (src, clip) = big_picture();
        let turn = Affine::rotate_about(0.2, 1400.0, 1150.0);
        let cases = [
            ("旋转并放大到 1.07 倍", turn.then(&Affine::scale_about(1.07, 1.07, 1400.0, 1150.0))),
            ("旋转并缩小到一半", turn.then(&Affine::scale_about(0.5, 0.5, 1400.0, 1150.0))),
            ("平移半个像素", Affine::translate(0.5, 0.5)),
            ("平移 3 个像素（照搬）", Affine::translate(3.0, 0.0)),
            ("恒等", Affine::IDENTITY),
        ];
        let profile = if cfg!(debug_assertions) { "dev" } else { "release" };
        for (name, m) in cases {
            let (fast, best) = (millis(&src, &m, Quality::Fast, clip), millis(&src, &m, Quality::Best, clip));
            eprintln!("[{profile}] {name}：Fast {fast:.2} ms，Best {best:.2} ms");
        }
    }
}
