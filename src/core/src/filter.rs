// SPDX-License-Identifier: GPL-3.0-or-later
//! 调色与模糊滤镜（规格 F-095，实现方案 §3.10）：色相/饱和度/明度、亮度/对比度、高斯模糊。
//!
//! 面板上每拖一下滑块，界面都拿图层原来的像素重新调用一次 `apply`，所以这里不留任何状态，
//! 也不改原图。结果是图层的一整张新分块表；滤镜没有改动的分块与原图共用，不复制。
//!
//! - 颜色值按存储的 sRGB 编码直接算，不换成线性光。
//! - 两种调色都是逐像素的：去掉预乘，改颜色，再按原来的不透明度乘回去。不透明度不变，全透明的像素不动。
//! - 模糊在预乘值上做：透明处的颜色是 0、权重也是 0，内容的边缘不会发黑。高斯核 σ = 半径 / 2，
//!   用三次盒式滤波串起来近似（Wells 1986 的级联均匀滤波），先横向、后纵向，各三次。
//!   盒子宽度取整数时三次的方差凑不准 σ²，小半径时差得最多（半径 1 会整个不模糊），拖滑块时模糊程度也是一跳一跳的。
//!   所以用 Gwosdek、Grewenig、Bruhn、Weickert 2011 的"扩展盒"：盒子两头再各带一个权重不足 1 的像素，
//!   三次的方差之和恰好是 σ²。两处出处是凭记忆写的，细节未核实。
//! - 模糊在画布边上把边缘像素向外延伸：铺满画布的单色图层怎么模糊都不变，边上也不会变淡。
//!   画布里面、内容以外的地方就是透明。
//!
//! 有选区时按选中程度在原图与滤镜结果之间过渡。保护不透明度时不透明度一位不差，
//! 颜色取滤镜结果、按原来的不透明度重新折算。
//!
//! 内存：模糊横向做完的中间结果按分块存，没有内容的分块不建，最多与图层本身一样大。
//! 此外每个线程只有两行（纵向时是两段 16 列宽的）浮点数草稿，画布高 16384 时约 8 MB；
//! 中间结果的分块列做完纵向就放掉。

use std::f64::consts::FRAC_PI_4;
use std::sync::Arc;

use crate::selection::Selection;
use crate::tile::{Tile, TileCoord, TileMap, TILE};
use crate::transform::content_bounds;
use crate::{Rect, ONE};

const T: usize = TILE as usize;
const N: usize = T * T;
/// 一个分块里通道值的个数。
const LEN: usize = N * 4;
/// 纵向模糊一次处理的列数。16 列共 64 个通道，每一行正好是 16 个四路浮点向量。
const GROUP: usize = 16;
const LANES: usize = GROUP * 4;
const _: () = assert!(T.is_multiple_of(GROUP));

/// 全透明的分块。原图里没有分块的位置拿它当原图，不用再分情况。
static BLANK: [u16; LEN] = [0; LEN];

/// 模糊半径的上限（像素）
const MAX_RADIUS: f32 = 100.0;
/// 对比度拉满时直线的斜率是无穷大。封顶在这里：离中灰差一点点的颜色也足够推到黑白两头，又不会算出无穷大和 NaN。
const MAX_SLOPE: f64 = 1e6;
/// 调色时分块不少于这么多才分给几个线程算。
const PARALLEL_TILES: usize = 16;

/// 一个滤镜和它的参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Adjust {
    /// 色相（度，−180..=180）、饱和度、明度（−1..=1）
    HueSat { hue: f32, saturation: f32, lightness: f32 },
    /// 亮度、对比度（−1..=1）
    BrightContrast { brightness: f32, contrast: f32 },
    /// 半径（像素，0..=100）
    Blur { radius: f32 },
}

impl Adjust {
    /// 参数都在零：什么都不改
    pub fn is_identity(&self) -> bool {
        match self.tidy() {
            Adjust::HueSat { hue, saturation, lightness } => hue == 0.0 && saturation == 0.0 && lightness == 0.0,
            Adjust::BrightContrast { brightness, contrast } => brightness == 0.0 && contrast == 0.0,
            Adjust::Blur { radius } => radius == 0.0,
        }
    }

    /// 参数收进各自的范围；不是数的当作 0。
    fn tidy(&self) -> Adjust {
        let fit = |v: f32, limit: f32| if v.is_nan() { 0.0 } else { v.clamp(-limit, limit) };
        match *self {
            Adjust::HueSat { hue, saturation, lightness } => {
                Adjust::HueSat { hue: fit(hue, 180.0), saturation: fit(saturation, 1.0), lightness: fit(lightness, 1.0) }
            }
            Adjust::BrightContrast { brightness, contrast } => {
                Adjust::BrightContrast { brightness: fit(brightness, 1.0), contrast: fit(contrast, 1.0) }
            }
            Adjust::Blur { radius } => Adjust::Blur { radius: fit(radius, MAX_RADIUS).max(0.0) },
        }
    }
}

/// 对图层像素做一次滤镜，返回图层的整张新分块表。`src` 是图层原来的像素，每次都从它算起。
///
/// - 有选区时只作用在选区里：结果 = 原图与滤镜结果按选中程度混合。
/// - `keep_alpha`（保护不透明度）：不透明度与原图一位不差；颜色取滤镜结果、按原来的不透明度折算，
///   滤镜结果在那里全透明时保留原像素。
/// - `clip` 是画布范围。clip 之外的源像素当作透明：没有选区时它们不进结果；有选区时它们不算选中，原样留着。
///   模糊在 clip 的边上把边缘像素向外延伸，不当作透明。
///
/// 参数都在零时原样返回（分块共用）。结果里没有全透明的分块，滤镜没改动的分块与 `src` 共用。
/// 计算分给几个线程，每个分块（模糊时每一条带、每一列）各算各的，结果与线程数无关。
pub fn apply(src: &TileMap, selection: Option<&Selection>, adjust: &Adjust, keep_alpha: bool, clip: Rect) -> TileMap {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    run(src, selection, adjust, keep_alpha, clip, threads)
}

/// 同 `apply`，线程数由调用的一方定（测试里拿来对照单线程和多线程的结果）。
fn run(src: &TileMap, selection: Option<&Selection>, adjust: &Adjust, keep_alpha: bool, clip: Rect, threads: usize) -> TileMap {
    let adjust = adjust.tidy();
    if adjust.is_identity() {
        return src.clone();
    }
    let ctx = Ctx { src, selection, keep_alpha, clip, threads };
    match adjust {
        Adjust::HueSat { hue, saturation, lightness } => {
            // 只动明度时不必绕道 HSL。
            let turn = hue != 0.0 || saturation != 0.0;
            recolour(&ctx, move |rgb| {
                let rgb = if turn { hue_sat(rgb, hue / 60.0, 1.0 + saturation) } else { rgb };
                rgb.map(|c| lighten(c, lightness))
            })
        }
        Adjust::BrightContrast { brightness, contrast } => {
            // 对比度绕中灰转动：−1 时斜率为 0（一片中灰），0 时为 1，接近 1 时趋于无穷（只剩黑白）。
            let slope = ((contrast as f64 + 1.0) * FRAC_PI_4).tan().clamp(0.0, MAX_SLOPE) as f32;
            recolour(&ctx, move |rgb| rgb.map(|c| ((lighten(c, brightness) - 0.5) * slope + 0.5).clamp(0.0, 1.0)))
        }
        Adjust::Blur { radius } => blur(&ctx, radius),
    }
}

/// 一次滤镜里各处都要用到的东西。
struct Ctx<'a> {
    src: &'a TileMap,
    selection: Option<&'a Selection>,
    keep_alpha: bool,
    clip: Rect,
    threads: usize,
}

impl Ctx<'_> {
    /// 结果可能变化的范围：画布之内，有选区时再限在选区的外接矩形里。
    fn area(&self) -> Rect {
        self.selection.map_or(self.clip, |s| s.bounds().intersect(self.clip))
    }

    /// 把算好的分块收起来。从原图出发：没算的分块（选区一点没碰到的、画布外的）原样留着；
    /// 算了的分块伸出画布的那部分也换回原来的像素（实现方案 §3.15）。
    fn assemble(&self, done: impl IntoIterator<Item = (TileCoord, Option<Arc<Tile>>)>) -> TileMap {
        let mut out = self.src.clone();
        for (tc, t) in done {
            out.replace(tc, crate::transform::keep_outside(t, self.src.get(tc), tc, self.clip));
        }
        out
    }
}

// ---- 调色 ----

/// 两种调色共用：逐像素去掉预乘，交给 `f` 改颜色（各分量 0..=1），再乘回原来的不透明度。
fn recolour(ctx: &Ctx, f: impl Fn([f32; 3]) -> [f32; 3] + Sync) -> TileMap {
    let area = ctx.area();
    // 只看原图里有的分块：透明处调完还是透明。有选区时，选区一点没碰到的分块也不用看。
    let coords: Vec<TileCoord> =
        ctx.src.coords().filter(|&tc| !tile_rect_in(tc, area).is_empty() && ctx.selection.is_none_or(|s| s.tile(tc).is_some())).collect();
    let done = parallel(
        coords,
        ctx.threads,
        PARALLEL_TILES,
        || vec![0u16; LEN],
        |tc, buf| {
            let r = tile_rect_in(tc, area);
            let orig = original(ctx.src, tc);
            for y in r.y0..r.y1 {
                let row = row_range(tc, r, y);
                for (d, p) in buf[row.clone()].as_chunks_mut::<4>().0.iter_mut().zip(orig[row].as_chunks::<4>().0) {
                    *d = unpremultiplied(*p, &f);
                }
            }
            (tc, finish(ctx, tc, r, buf))
        },
    );
    ctx.assemble(done)
}

/// 去掉预乘交给 f 改颜色，再按原来的不透明度乘回去。全透明的像素没有颜色可改，原样返回。
#[inline]
fn unpremultiplied(p: [u16; 4], f: &impl Fn([f32; 3]) -> [f32; 3]) -> [u16; 4] {
    let a = p[3];
    if a == 0 {
        return p;
    }
    let (alpha, inv) = (a as f32, 1.0 / a as f32);
    let rgb = f([0, 1, 2].map(|c| (p[c] as f32 * inv).min(1.0)));
    // 颜色不超过 1，乘回去不会超过不透明度；f 没改动的颜色乘回去恰好是原来的值。
    let back = |v: f32| ((v.clamp(0.0, 1.0) * alpha + 0.5) as u16).min(a);
    [back(rgb[0]), back(rgb[1]), back(rgb[2]), a]
}

/// 换到 HSL（Joblove、Greenberg 1978 的双六棱锥，出处凭记忆），色相转 `turn` 个六分之一圈，
/// 饱和度乘 `gain`（不超过 1），再换回来。
#[inline]
fn hue_sat([r, g, b]: [f32; 3], turn: f32, gain: f32) -> [f32; 3] {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) * 0.5;
    let d = max - min;
    // 灰色没有色相可转，饱和度 0 乘多少还是 0。
    if d <= 0.0 {
        return [l; 3];
    }
    // 饱和度：色度与这个明度下可能的最大色度之比。分母只在纯黑、纯白处为 0，那时 d 也是 0。
    let s = (d / (1.0 - (max + min - 1.0).abs())).min(1.0);
    let h = if max == r {
        (g - b) / d
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    // h 在 [−1, 5) 里，转动量在 [−3, 3] 里，加起来折回 [0, 6) 只要一步。浮点取余是函数调用，逐像素用太慢。
    let mut h = h + turn;
    if h < 0.0 {
        h += 6.0;
    } else if h >= 6.0 {
        h -= 6.0;
    }
    // 落在第几个六分之一圈。舍入可能让 h 恰好等于 6，归进最后一段，算出来与 h = 0 相同。
    let k = (h as u32).min(5);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * (s * gain).clamp(0.0, 1.0);
    let x = c * (1.0 - (h - (k & !1) as f32 - 1.0).abs());
    let m = l - c * 0.5;
    let (r, g, b) = match k {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r + m, g + m, b + m]
}

/// 明度、亮度：正值把颜色按比例推向白，负值按比例推向黑。±1 时是纯白、纯黑。
#[inline]
fn lighten(c: f32, l: f32) -> f32 {
    if l > 0.0 {
        c + (1.0 - c) * l
    } else {
        c * (1.0 + l)
    }
}

// ---- 模糊 ----

/// 扩展盒式滤波的一次：中心两边各 r 个像素权重为 1，再往外各一个像素权重为 alpha（0..1），总和归一。
#[derive(Clone, Copy)]
struct Boxes {
    r: usize,
    alpha: f32,
    norm: f32,
}

impl Boxes {
    /// 三次串起来的方差之和是 σ²。
    fn new(sigma: f64) -> Self {
        // 每次分到三分之一的方差 v。宽 2r+1 的盒子方差是 r(r+1)/3：r 取不超过 v 的最大整数，
        // 差的那一点由两头的小权重补上，解 (r(r+1)(2r+1)/3 + 2α(r+1)²) / (2r+1+2α) = v 得 α。
        let v = sigma * sigma / 3.0;
        let r = (((1.0 + 12.0 * v).sqrt() - 1.0) / 2.0).floor().max(0.0);
        let alpha = ((2.0 * r + 1.0) * (v - r * (r + 1.0) / 3.0) / (2.0 * ((r + 1.0) * (r + 1.0) - v))).clamp(0.0, 1.0);
        Boxes { r: r as usize, alpha: alpha as f32, norm: (1.0 / (2.0 * r + 1.0 + 2.0 * alpha)) as f32 }
    }

    /// 一次滤波向两边各够到几个像素
    fn reach(&self) -> usize {
        self.r + 1
    }
}

/// 一条带（横向）或一列（纵向）上的中间结果，一格一个分块，整块是 0 的没有建。
type Strip = Vec<Option<Vec<u16>>>;

/// 一次模糊的范围和核。
struct Blur<'a> {
    ctx: &'a Ctx<'a>,
    k: Boxes,
    /// 三次滤波加起来向两边各够到几个像素
    reach: usize,
    /// 要写结果的范围
    out: Rect,
    /// 要读的源像素：out 向外扩 reach，裁到画布里
    inner: Rect,
}

/// 一个线程的草稿：两段浮点数，滤波时来回倒。
struct Lines<const L: usize> {
    a: Vec<[f32; L]>,
    b: Vec<[f32; L]>,
}

impl<const L: usize> Lines<L> {
    fn new(n: usize) -> Self {
        Lines { a: vec![[0.0; L]; n], b: vec![[0.0; L]; n] }
    }
}

/// 纵向那一遍的草稿，外加正在写的这一列输出分块。
struct Columns {
    lines: Lines<LANES>,
    tiles: Vec<Vec<u16>>,
}

fn blur(ctx: &Ctx, radius: f32) -> TileMap {
    let k = Boxes::new(radius as f64 / 2.0);
    let reach = 3 * k.reach();
    let content = content_bounds(ctx.src, ctx.clip);
    // 保护不透明度时透明处始终透明，结果只会落在原有内容的范围里。
    let out = grow(content, if ctx.keep_alpha { 0 } else { reach }).intersect(ctx.area());
    if out.is_empty() {
        return ctx.assemble(Vec::new());
    }
    let job = Blur { ctx, k, reach, out, inner: grow(out, reach).intersect(ctx.clip) };

    // 横向：一个分块行一条带，结果按分块存。
    let bands: Vec<i32> = (job.inner.y0.div_euclid(TILE)..=(job.inner.y1 - 1).div_euclid(TILE)).collect();
    let width = (out.x1 - out.x0) as usize + 2 * reach;
    let bands = parallel(bands, ctx.threads, 2, || Lines::<4>::new(width), |ty, s| job.horizontal(ty, s));

    // 转成一列一列的，纵向时每列交给一个线程，做完就放掉。
    let (tx0, tx1) = (out.x0.div_euclid(TILE), (out.x1 - 1).div_euclid(TILE));
    let mut columns: Vec<Strip> = (tx0..=tx1).map(|_| Vec::with_capacity(bands.len())).collect();
    for band in bands {
        for (column, t) in columns.iter_mut().zip(band) {
            column.push(t);
        }
    }
    let height = (out.y1 - out.y0) as usize + 2 * reach;
    let scratch = || Columns { lines: Lines::new(height), tiles: Vec::new() };
    let done = parallel((tx0..=tx1).zip(columns).collect(), ctx.threads, 2, scratch, |(tx, column), s| job.vertical(tx, column, s));
    ctx.assemble(done.into_iter().flatten())
}

impl Blur<'_> {
    /// 分块行 ty 上的横向模糊：每一行读 [out.x0 − reach, out.x1 + reach) 的源像素，写出 [out.x0, out.x1) 的结果。
    fn horizontal(&self, ty: i32, s: &mut Lines<4>) -> Strip {
        let (out, inner) = (self.out, self.inner);
        let tx0 = out.x0.div_euclid(TILE);
        let mut band: Strip = vec![None; ((out.x1 - 1).div_euclid(TILE) - tx0 + 1) as usize];
        let sx0 = inner.x0.div_euclid(TILE);
        let tiles: Vec<Option<&[u16; LEN]>> =
            (sx0..=(inner.x1 - 1).div_euclid(TILE)).map(|tx| self.ctx.src.get((tx, ty)).and_then(|t| pixels(t))).collect();
        if tiles.iter().all(Option::is_none) {
            return band;
        }
        // 行首要在源像素前面补几格；只有够到画布左边外面时才补。
        let left = (inner.x0 as i64 - (out.x0 as i64 - self.reach as i64)) as usize;
        let width = (inner.x1 - inner.x0) as usize;
        let n = (out.x1 - out.x0) as usize + 2 * self.reach;
        let (y0, y1) = span(ty, inner.y0, inner.y1);
        for y in y0..y1 {
            let ly = y.rem_euclid(TILE) as usize;
            let a = &mut s.a[..n];
            let mut k = left;
            for (i, t) in tiles.iter().enumerate() {
                let (x0, x1) = span(sx0 + i as i32, inner.x0, inner.x1);
                let seg = &mut a[k..k + (x1 - x0) as usize];
                match t {
                    Some(t) => {
                        let o = (ly * T + x0.rem_euclid(TILE) as usize) * 4;
                        for (d, p) in seg.iter_mut().zip(t[o..].as_chunks::<4>().0) {
                            *d = p.map(f32::from);
                        }
                    }
                    None => seg.fill([0.0; 4]),
                }
                k += (x1 - x0) as usize;
            }
            // 画布边上：边缘像素向外延伸。
            let (first, last) = (a[left], a[left + width - 1]);
            a[..left].fill(first);
            a[left + width..].fill(last);
            let m = three_boxes(a, &mut s.b[..n], &self.k);
            self.store(&s.b[..m], ly, &mut band);
        }
        band
    }

    /// 横向做完的一行写进这条带的分块里。整段是 0 的分块先不建。
    fn store(&self, row: &[[f32; 4]], ly: usize, band: &mut [Option<Vec<u16>>]) {
        let out = self.out;
        let tx0 = out.x0.div_euclid(TILE);
        for (i, slot) in band.iter_mut().enumerate() {
            let (x0, x1) = span(tx0 + i as i32, out.x0, out.x1);
            let seg = &row[(x0 - out.x0) as usize..(x1 - out.x0) as usize];
            if slot.is_none() {
                if seg.iter().all(|v| pack(v)[3] == 0) {
                    continue;
                }
                *slot = Some(vec![0; LEN]);
            }
            let Some(t) = slot else { continue };
            let o = (ly * T + x0.rem_euclid(TILE) as usize) * 4;
            for (d, v) in t[o..].as_chunks_mut::<4>().0.iter_mut().zip(seg) {
                *d = pack(v);
            }
        }
    }

    /// 分块列 tx 上的纵向模糊，接着把这一列的输出分块收尾。
    fn vertical(&self, tx: i32, column: Strip, s: &mut Columns) -> Vec<(TileCoord, Option<Arc<Tile>>)> {
        let (out, clip) = (self.out, self.ctx.clip);
        let (ty0, ty1) = (out.y0.div_euclid(TILE), (out.y1 - 1).div_euclid(TILE));
        let count = (ty1 - ty0 + 1) as usize;
        s.tiles.resize_with(count, || vec![0; LEN]);
        for t in &mut s.tiles[..count] {
            t.fill(0);
        }
        let band0 = self.inner.y0.div_euclid(TILE);
        let (xa, xb) = span(tx, out.x0, out.x1);
        let n = (out.y1 - out.y0) as usize + 2 * self.reach;
        // 整列都没有内容时结果就是 0，不用算。
        let groups = if column.iter().any(Option::is_some) { T / GROUP } else { 0 };
        for g in 0..groups {
            let gx = (tx as i64 * TILE as i64 + (g * GROUP) as i64) as i32;
            let (wa, wb) = (gx.max(xa), (gx as i64 + GROUP as i64).min(xb as i64) as i32);
            if wa >= wb {
                continue;
            }
            let a = &mut s.lines.a[..n];
            for (j, d) in a.iter_mut().enumerate() {
                // 画布外面的行由边上那一行代替。
                let y = (out.y0 as i64 - self.reach as i64 + j as i64).clamp(clip.y0 as i64, clip.y1 as i64 - 1) as i32;
                match &column[(y.div_euclid(TILE) - band0) as usize] {
                    Some(t) => {
                        let o = (y.rem_euclid(TILE) as usize * T + g * GROUP) * 4;
                        for (v, &c) in d.iter_mut().zip(&t[o..o + LANES]) {
                            *v = c as f32;
                        }
                    }
                    None => *d = [0.0; LANES],
                }
            }
            let m = three_boxes(a, &mut s.lines.b[..n], &self.k);
            for (j, v) in s.lines.b[..m].iter().enumerate() {
                let y = out.y0 + j as i32;
                let t = &mut s.tiles[(y.div_euclid(TILE) - ty0) as usize];
                let o = (y.rem_euclid(TILE) as usize * T + wa.rem_euclid(TILE) as usize) * 4;
                let px = &v.as_chunks::<4>().0[(wa - gx) as usize..(wb - gx) as usize];
                for (d, p) in t[o..].as_chunks_mut::<4>().0.iter_mut().zip(px) {
                    *d = pack(p);
                }
            }
        }
        drop(column);
        (0..count)
            .map(|i| {
                let tc = (tx, ty0 + i as i32);
                (tc, finish(self.ctx, tc, tile_rect_in(tc, out), &mut s.tiles[i]))
            })
            .collect()
    }
}

/// 三次扩展盒式滤波。`a` 是输入，两头各比要的结果多出 3(r+1) 格；结果在 `b` 的开头，返回它的长度。`a` 被当作草稿弄乱。
fn three_boxes<const L: usize>(a: &mut [[f32; L]], b: &mut [[f32; L]], k: &Boxes) -> usize {
    let (n, h) = (a.len(), 2 * k.reach());
    box_pass(&a[..n], &mut b[..n - h], k);
    box_pass(&b[..n - h], &mut a[..n - 2 * h], k);
    box_pass(&a[..n - 2 * h], &mut b[..n - 3 * h], k);
    n - 3 * h
}

/// 一次扩展盒式滤波。`dst` 比 `src` 两头各短 r+1 格：dst[i] 以 src[i + r + 1] 为中心。
/// 中间 2r+1 格的和沿途加一格、减一格地滚动，每格的工作量与半径无关。
/// 每个通道走的是同一串运算，四个通道相同的像素（比如白色）滤完四个通道仍然相同。
#[inline]
fn box_pass<const L: usize>(src: &[[f32; L]], dst: &mut [[f32; L]], k: &Boxes) {
    let w = 2 * k.r + 1;
    debug_assert_eq!(dst.len() + w + 1, src.len());
    let mut sum = [0f32; L];
    for p in &src[1..=w] {
        sum = std::array::from_fn(|l| sum[l] + p[l]);
    }
    for (i, out) in dst.iter_mut().enumerate() {
        let (lo, hi, gone) = (&src[i], &src[i + w + 1], &src[i + 1]);
        *out = std::array::from_fn(|l| (sum[l] + k.alpha * (lo[l] + hi[l])) * k.norm);
        sum = std::array::from_fn(|l| sum[l] + (hi[l] - gone[l]));
    }
}

/// 浮点的预乘值收成合法的像素：四舍五入，0 ≤ 颜色 ≤ 不透明度 ≤ ONE。
/// 滚动求和的舍入误差会让本该是 0 的地方带一点正负的零头，在这里一并收掉。
#[inline]
fn pack(v: &[f32; 4]) -> [u16; 4] {
    let round = |v: f32, max: f32| (v + 0.5).clamp(0.0, max) as u16;
    let a = round(v[3], ONE as f32);
    [round(v[0], a as f32), round(v[1], a as f32), round(v[2], a as f32), a]
}

// ---- 收尾 ----

/// 一个分块的收尾。`buf` 在 `r` 的范围里是滤镜的结果，这里按"保护不透明度"和选中程度与原图合起来；
/// r 之外放回原图，没有选区时 clip 之外清空。返回这一格在结果里放什么：
/// 与原图逐像素相同就共用原来的分块，全透明就什么也不放。
fn finish(ctx: &Ctx, tc: TileCoord, r: Rect, buf: &mut Vec<u16>) -> Option<Arc<Tile>> {
    let whole = ctx.src.get(tc);
    let orig = original(ctx.src, tc);
    let cover = match ctx.selection {
        None => None,
        Some(s) => match s.tile(tc) {
            Some(m) if m.len() == N => Some(m),
            // 这一格一点没选中。
            _ => return whole.cloned(),
        },
    };
    let [x0, y0, x1, y1] = local(tc, r);
    for ly in 0..T {
        let row = ly * T * 4..(ly + 1) * T * 4;
        let (a, b) = if (y0..y1).contains(&ly) { (x0, x1) } else { (0, 0) };
        buf[row.start..row.start + a * 4].copy_from_slice(&orig[row.start..row.start + a * 4]);
        buf[row.start + b * 4..row.end].copy_from_slice(&orig[row.start + b * 4..row.end]);
        // 全选中又不保护不透明度时，结果就是滤镜的结果。
        if cover.is_none() && !ctx.keep_alpha {
            continue;
        }
        for i in a..b {
            let o = (ly * T + i) * 4;
            let v = cover.map_or(ONE, |m| (m[ly * T + i] as u32).min(ONE));
            let px = combine(rgba(orig, o), rgba(buf, o), v, ctx.keep_alpha);
            buf[o..o + 4].copy_from_slice(&px);
        }
    }
    if ctx.selection.is_none() {
        // 没有选区时画布外面不留东西。
        let [cx0, cy0, cx1, cy1] = local(tc, tile_rect_in(tc, ctx.clip));
        for (ly, row) in buf.as_chunks_mut::<{ T * 4 }>().0.iter_mut().enumerate() {
            if (cy0..cy1).contains(&ly) {
                row[..cx0 * 4].fill(0);
                row[cx1 * 4..].fill(0);
            } else {
                row.fill(0);
            }
        }
    }
    if whole.is_some_and(|t| t.0 == *buf) {
        return whole.cloned();
    }
    if !visible(buf) {
        return None;
    }
    // 写好的缓冲区直接交出去，换一块接着用。
    Some(Arc::new(Tile(std::mem::replace(buf, vec![0; LEN]))))
}

/// 原像素 o 与滤镜结果 f 按选中程度 v（0..=ONE）合起来。
#[inline]
fn combine(o: [u16; 4], f: [u16; 4], v: u32, keep_alpha: bool) -> [u16; 4] {
    let f = if keep_alpha { protect(o, f) } else { f };
    if v >= ONE {
        return f;
    }
    if v == 0 {
        return o;
    }
    // o + (f − o)·v，四舍五入：f 与 o 相同的通道恰好不变，所以调色时不透明度一位不差。
    let mix = |a: u16, b: u16| (a as i32 + (((b as i32 - a as i32) * v as i32 + (ONE / 2) as i32) >> 15)) as u32;
    let alpha = mix(o[3], f[3]).min(ONE);
    // 各通道分别舍入，颜色可能比不透明度多出 1，压回去。
    [mix(o[0], f[0]).min(alpha) as u16, mix(o[1], f[1]).min(alpha) as u16, mix(o[2], f[2]).min(alpha) as u16, alpha as u16]
}

/// 保护不透明度：不透明度用原来的，颜色按滤镜结果里"颜色与不透明度之比"折算过来。
/// 滤镜结果在这里全透明时没有颜色可取，保留原像素。
#[inline]
fn protect(o: [u16; 4], f: [u16; 4]) -> [u16; 4] {
    if f[3] == o[3] {
        return f;
    }
    if f[3] == 0 {
        return o;
    }
    let (oa, fa) = (o[3] as u32, f[3] as u32);
    let c = |k: usize| ((f[k] as u32 * oa + fa / 2) / fa).min(oa) as u16;
    [c(0), c(1), c(2), o[3]]
}

// ---- 公用的小工具 ----

/// 把 `items` 轮流分给 `threads` 个线程，每个线程用 `scratch` 建一份自己的草稿；少于 `least` 件时就在当前线程做。
/// 结果按 `items` 原来的顺序排好。每件事各算各的，分给几个线程算，结果逐像素相同。
fn parallel<I: Send, R: Send, S>(
    items: Vec<I>,
    threads: usize,
    least: usize,
    scratch: impl Fn() -> S + Sync,
    work: impl Fn(I, &mut S) -> R + Sync,
) -> Vec<R> {
    let threads = threads.min(items.len());
    if threads <= 1 || items.len() < least {
        let mut s = scratch();
        return items.into_iter().map(|i| work(i, &mut s)).collect();
    }
    // 相邻的几件分给不同的线程：内容常常集中在画面的一块，这样各线程的活儿差不多。
    let mut lanes: Vec<Vec<(usize, I)>> = (0..threads).map(|_| Vec::new()).collect();
    for (k, item) in items.into_iter().enumerate() {
        lanes[k % threads].push((k, item));
    }
    let (scratch, work) = (&scratch, &work);
    let mut done: Vec<(usize, R)> = std::thread::scope(|scope| {
        let jobs: Vec<_> = lanes
            .into_iter()
            .map(|lane| {
                scope.spawn(move || {
                    let mut s = scratch();
                    lane.into_iter().map(|(k, i)| (k, work(i, &mut s))).collect::<Vec<_>>()
                })
            })
            .collect();
        jobs.into_iter().flat_map(|j| j.join().unwrap()).collect()
    });
    done.sort_unstable_by_key(|d| d.0);
    done.into_iter().map(|d| d.1).collect()
}

/// 矩形向四周各扩 d 个像素。
fn grow(r: Rect, d: usize) -> Rect {
    if r.is_empty() {
        return Rect::EMPTY;
    }
    let d = d.min(i32::MAX as usize) as i32;
    Rect { x0: r.x0.saturating_sub(d), y0: r.y0.saturating_sub(d), x1: r.x1.saturating_add(d), y1: r.y1.saturating_add(d) }
}

/// 第 t 个分块（横向或纵向）落在 [lo, hi) 里的那一段。用 64 位算，坐标再离谱也不会溢出。
fn span(t: i32, lo: i32, hi: i32) -> (i32, i32) {
    let o = t as i64 * TILE as i64;
    (o.max(lo as i64) as i32, (o + TILE as i64).min(hi as i64) as i32)
}

/// 原图在 tc 处的像素；没有这个分块（或分块长度不对）时是全透明的。
fn original(src: &TileMap, tc: TileCoord) -> &[u16; LEN] {
    src.get(tc).and_then(|t| pixels(t)).unwrap_or(&BLANK)
}

/// 分块的像素数据。`Tile` 里的 Vec 是公开的，长度不对的（不该有）一律当作没有这个分块。
fn pixels(t: &Tile) -> Option<&[u16; LEN]> {
    t.0.as_slice().try_into().ok()
}

fn visible(data: &[u16]) -> bool {
    data.as_chunks::<4>().0.iter().any(|p| p[3] != 0)
}

#[inline]
fn rgba(s: &[u16], o: usize) -> [u16; 4] {
    let p = &s[o..o + 4];
    [p[0], p[1], p[2], p[3]]
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

/// r（必须在分块 tc 之内）在分块里的局部范围 [x0, y0, x1, y1]；r 是空的时候全为 0。
fn local(tc: TileCoord, r: Rect) -> [usize; 4] {
    if r.is_empty() {
        return [0; 4];
    }
    let (ox, oy) = (tc.0 as i64 * TILE as i64, tc.1 as i64 * TILE as i64);
    [(r.x0 as i64 - ox) as usize, (r.y0 as i64 - oy) as usize, (r.x1 as i64 - ox) as usize, (r.y1 as i64 - oy) as usize]
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
    use crate::tile::{tiles_in, MaskMap};
    use std::time::Instant;

    const H: u16 = (ONE / 2) as u16;
    const FULL: u16 = ONE as u16;
    const RED: [u16; 4] = [FULL, 0, 0, FULL];

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

    /// 两张图的分块完全是同一批（共用，没有复制）。
    fn shared(a: &TileMap, b: &TileMap) -> bool {
        a.len() == b.len() && a.coords().all(|tc| b.get(tc).is_some_and(|t| Arc::ptr_eq(t, a.get(tc).unwrap())))
    }

    /// 两张图的分块一一对应、内容逐个相同。
    fn same(a: &TileMap, b: &TileMap) -> bool {
        a.len() == b.len() && a.coords().all(|tc| b.get(tc).is_some_and(|t| t.0 == a.get(tc).unwrap().0))
    }

    fn near(p: [u16; 4], q: [u16; 4], tolerance: u16) -> bool {
        p.iter().zip(q).all(|(&a, b)| a.abs_diff(b) <= tolerance)
    }

    /// 选中程度在 [x0, split) 是满的、在 [split, x1) 是一半的选区。
    fn half_selection(r: Rect, split: i32) -> Selection {
        let mut mask = MaskMap::default();
        for (x, y) in pixels_in(r) {
            mask.get_mut((x.div_euclid(TILE), y.div_euclid(TILE)))[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize] =
                if x < split { FULL } else { H };
        }
        Selection::from_mask(mask, r)
    }

    #[test]
    fn zero_parameters_change_nothing() {
        let clip = rect(0, 0, 300, 200);
        // 内容伸出了画布：参数为零时连画布外面的部分也原样留着。
        let src = noise_in(rect(-20, 30, 250, 220));
        let sel = half_selection(rect(40, 40, 120, 90), 80);
        let zero = [
            Adjust::HueSat { hue: 0.0, saturation: 0.0, lightness: 0.0 },
            Adjust::BrightContrast { brightness: 0.0, contrast: -0.0 },
            Adjust::Blur { radius: 0.0 },
            // 不是数的参数当作 0，负的半径当作 0。
            Adjust::HueSat { hue: f32::NAN, saturation: 0.0, lightness: f32::NAN },
            Adjust::Blur { radius: -3.0 },
            Adjust::Blur { radius: f32::NAN },
        ];
        for adjust in zero {
            assert!(adjust.is_identity(), "{adjust:?}");
            for s in [None, Some(&sel)] {
                for keep_alpha in [false, true] {
                    assert!(shared(&apply(&src, s, &adjust, keep_alpha, clip), &src), "{adjust:?}");
                }
            }
        }
        let not_zero = [
            Adjust::HueSat { hue: 1.0, saturation: 0.0, lightness: 0.0 },
            Adjust::HueSat { hue: 0.0, saturation: 0.01, lightness: 0.0 },
            Adjust::HueSat { hue: 0.0, saturation: 0.0, lightness: -0.5 },
            Adjust::BrightContrast { brightness: 0.1, contrast: 0.0 },
            Adjust::BrightContrast { brightness: 0.0, contrast: -1.0 },
            Adjust::Blur { radius: 0.5 },
        ];
        assert!(not_zero.iter().all(|a| !a.is_identity()));
    }

    #[test]
    fn hue_saturation_and_lightness() {
        let clip = rect(0, 0, 130, 70);
        let mut src = TileMap::default();
        // 左边不透明的红，中间半透明的红，右边各种噪声。
        paint(&mut src, clip, |x, y| match x {
            0..40 => RED,
            40..65 => [H, 0, 0, H],
            _ => noise(x, y),
        });
        let hs = |hue, saturation, lightness| apply(&src, None, &Adjust::HueSat { hue, saturation, lightness }, false, clip);

        // 红转 180° 是青，转 120° 是绿，转 −120° 是蓝；半透明的转完仍是半透明。
        for (hue, want) in [(180.0, [0, FULL, FULL]), (-180.0, [0, FULL, FULL]), (120.0, [0, FULL, 0]), (-120.0, [0, 0, FULL])] {
            let out = hs(hue, 0.0, 0.0);
            let p = at(&out, 10, 10);
            assert!(near(p, [want[0], want[1], want[2], FULL], 2), "{hue}°：{p:?}");
            let q = at(&out, 50, 10);
            assert!(near(q, [want[0] / 2, want[1] / 2, want[2] / 2, H], 2), "{hue}°，半透明：{q:?}");
        }
        // 饱和度 −1：灰。红变成明度一半的灰。
        let grey = hs(0.0, -1.0, 0.0);
        assert!(pixels_in(clip).all(|(x, y)| {
            let p = at(&grey, x, y);
            p[0] == p[1] && p[1] == p[2]
        }));
        assert!(near(at(&grey, 10, 10), [H, H, H, FULL], 1));
        // 明度 +1 全白，−1 全黑；不透明度不变。
        let (white, black) = (hs(0.0, 0.0, 1.0), hs(0.0, 0.0, -1.0));
        for (x, y) in pixels_in(clip) {
            let a = at(&src, x, y)[3];
            assert_eq!(at(&white, x, y), [a; 4], "({x}, {y})");
            assert_eq!(at(&black, x, y), [0, 0, 0, a], "({x}, {y})");
        }
        // 不透明度从不改变，半透明的像素仍是合法的预乘值。
        for (h, s, l) in [(30.0, 0.2, 0.1), (-150.0, 1.0, -0.4), (77.0, -0.6, 0.9), (5.0, 0.0, 0.0)] {
            let out = hs(h, s, l);
            assert_tidy(&out, clip);
            assert!(pixels_in(clip).all(|(x, y)| at(&out, x, y)[3] == at(&src, x, y)[3]), "{h} {s} {l}");
        }

        // 转过去再转回来：六段色相都走一遍，差不出几个单位（两次舍入）。
        for turn in [75.0, 150.0, -20.0] {
            let there = hs(turn, 0.0, 0.0);
            let back = apply(&there, None, &Adjust::HueSat { hue: -turn, saturation: 0.0, lightness: 0.0 }, false, clip);
            for (x, y) in pixels_in(clip) {
                assert!(near(at(&back, x, y), at(&src, x, y), 3), "{turn}° ({x}, {y})：{:?} 对 {:?}", at(&back, x, y), at(&src, x, y));
            }
        }

        // 灰色没有色相：转色相、加饱和度都一个像素不变，分块全部共用。
        let mut lines = TileMap::default();
        paint(&mut lines, clip, |x, y| {
            let a = noise(x, y)[3];
            [a / 3, a / 3, a / 3, a]
        });
        for adjust in [Adjust::HueSat { hue: 90.0, saturation: 0.0, lightness: 0.0 }, Adjust::HueSat { hue: -45.0, saturation: 0.7, lightness: 0.0 }]
        {
            assert!(shared(&apply(&lines, None, &adjust, false, clip), &lines));
        }
    }

    #[test]
    fn brightness_and_contrast() {
        let clip = rect(0, 0, 64, 64);
        let level = |v: f32, a: u16| {
            let c = (v * a as f32).round() as u16;
            [c, c, c, a]
        };
        let mut src = TileMap::default();
        // 左暗右亮；下半边半透明。
        paint(&mut src, clip, |x, y| level(if x < 32 { 0.25 } else { 0.75 }, if y < 32 { FULL } else { H }));
        let bc = |brightness, contrast| apply(&src, None, &Adjust::BrightContrast { brightness, contrast }, false, clip);
        let (dark, light) = (at(&src, 0, 0)[0], at(&src, 40, 0)[0]);

        // 加对比度：暗的更暗，亮的更亮。斜率 tan 67.5° ≈ 2.4，0.25 和 0.75 正好推到头。
        let more = bc(0.0, 0.5);
        assert_eq!(at(&more, 0, 0), [0, 0, 0, FULL]);
        assert_eq!(at(&more, 40, 0), [FULL; 4]);
        assert_eq!(at(&more, 40, 40), [H; 4]);
        let some = bc(0.0, 0.1);
        assert!(at(&some, 0, 0)[0] < dark && at(&some, 40, 0)[0] > light);
        // 减对比度：向中灰靠拢，斜率 tan 22.5° ≈ 0.41。
        let less = bc(0.0, -0.5);
        let want = ((0.5 - 0.25 * (std::f64::consts::PI / 8.0).tan()) * ONE as f64).round() as u16;
        assert!(near(at(&less, 0, 0), [want, want, want, FULL], 1), "{:?}", at(&less, 0, 0));
        // 对比度 −1 是一片中灰，+1 只剩黑白，都不会算出 NaN。
        let flat = bc(0.0, -1.0);
        assert!(pixels_in(clip).all(|(x, y)| at(&flat, x, y) == level(0.5, at(&src, x, y)[3])));
        let hard = bc(0.0, 1.0);
        assert_tidy(&hard, clip);
        assert_eq!((at(&hard, 0, 0), at(&hard, 40, 0)), ([0, 0, 0, FULL], [FULL; 4]));
        // 亮度 −1 全黑，+1 全白；半透明的仍是半透明。
        let (black, white) = (bc(-1.0, 0.0), bc(1.0, 0.0));
        assert_eq!((at(&black, 40, 0), at(&black, 40, 40)), ([0, 0, 0, FULL], [0, 0, 0, H]));
        assert_eq!((at(&white, 0, 0), at(&white, 0, 40)), ([FULL; 4], [H; 4]));
        // 先调亮度再调对比度：亮度 +0.2 把 0.25 提到 0.4，再按斜率 tan(0.3·π/4 + π/4) 拉开。
        let both = bc(0.2, 0.3);
        let slope = (1.3 * std::f64::consts::FRAC_PI_4).tan();
        let want = (((0.4 - 0.5) * slope + 0.5) * ONE as f64).round() as u16;
        assert!(near(at(&both, 0, 0), [want, want, want, FULL], 2), "{:?} 对 {want}", at(&both, 0, 0));
        for (b, c) in [(0.3, 0.6), (-0.7, -0.2), (0.0, 0.9)] {
            let out = bc(b, c);
            assert_tidy(&out, clip);
            assert!(pixels_in(clip).all(|(x, y)| at(&out, x, y)[3] == at(&src, x, y)[3]));
        }
    }

    #[test]
    fn selection_limits_the_effect() {
        let clip = rect(0, 0, 200, 150);
        let mut src = TileMap::default();
        paint(&mut src, clip, |x, y| if y < 75 { RED } else { noise(x, y) });
        // [20, 100) 全选中，[100, 130) 选中一半。
        let sel = half_selection(rect(20, 10, 130, 140), 100);
        let cases = [
            Adjust::HueSat { hue: 180.0, saturation: 0.0, lightness: 0.0 },
            Adjust::BrightContrast { brightness: 0.3, contrast: 0.4 },
            Adjust::Blur { radius: 6.0 },
        ];
        for adjust in cases {
            for keep_alpha in [false, true] {
                let out = apply(&src, Some(&sel), &adjust, keep_alpha, clip);
                // 选区外面一位不差，模糊够得着的地方也一样。
                for (x, y) in pixels_in(clip) {
                    if sel.get(x, y) == 0 {
                        assert_eq!(at(&out, x, y), at(&src, x, y), "{adjust:?} ({x}, {y})");
                    }
                }
                // 选区碰不到的分块连复制都没有发生。
                for tc in [(3, 0), (3, 2)] {
                    assert!(Arc::ptr_eq(out.get(tc).unwrap(), src.get(tc).unwrap()), "{adjust:?} {tc:?}");
                }
                assert_tidy(&out, clip);
            }
        }
        // 全选中的地方红变青，选中一半的地方停在半路。
        let out = apply(&src, Some(&sel), &cases[0], false, clip);
        assert_eq!(at(&out, 50, 20), [0, FULL, FULL, FULL]);
        assert_eq!(at(&out, 110, 20), [H, H, H, FULL]);
        assert_eq!(at(&out, 10, 20), RED);
        // 空选区：什么都不改。
        assert!(shared(&apply(&src, Some(&Selection::empty()), &cases[2], false, clip), &src));
    }

    #[test]
    fn protecting_opacity_keeps_alpha_exactly() {
        let clip = rect(0, 0, 160, 120);
        // 噪声里夹着全透明的像素。
        let src = noise_in(rect(10, 10, 150, 110));
        let sel = half_selection(rect(30, 0, 120, 120), 80);
        let cases = [
            Adjust::Blur { radius: 4.0 },
            Adjust::Blur { radius: 40.0 },
            Adjust::HueSat { hue: -60.0, saturation: 0.5, lightness: 0.3 },
            Adjust::BrightContrast { brightness: -0.4, contrast: 0.7 },
        ];
        for adjust in cases {
            for s in [None, Some(&sel)] {
                let out = apply(&src, s, &adjust, true, clip);
                assert_tidy(&out, clip);
                assert!(pixels_in(rect(0, 0, 192, 128)).all(|(x, y)| at(&out, x, y)[3] == at(&src, x, y)[3]), "{adjust:?}");
                // 颜色确实变了。
                assert!(pixels_in(clip).any(|(x, y)| at(&out, x, y) != at(&src, x, y)), "{adjust:?}");
            }
        }
        // 红蓝两块相邻：保护不透明度时交界处的颜色互相渗过去，不透明度不变；透明处仍是透明。
        let mut two = TileMap::default();
        paint(&mut two, rect(40, 40, 120, 80), |x, _| if x < 80 { RED } else { [0, 0, FULL, FULL] });
        let out = apply(&two, None, &Adjust::Blur { radius: 8.0 }, true, clip);
        let p = at(&out, 79, 60);
        assert!(p[3] == FULL && p[0] > 0 && p[2] > 0, "{p:?}");
        assert_eq!(at(&out, 40, 40), RED, "角上只挨着红色，透明的邻居不算数");
        assert_eq!(at(&out, 39, 60), [0; 4]);
        assert_eq!(alpha_sum(&out, clip), alpha_sum(&two, clip));
    }

    #[test]
    fn solid_layer_stays_solid_under_blur() {
        // 画布的边不在分块边界上。
        let clip = rect(0, 0, 300, 230);
        for colour in [[3000, 9000, 12000, 20000], [FULL; 4], [100, 0, 30000, FULL]] {
            let mut src = TileMap::default();
            paint(&mut src, clip, |_, _| colour);
            for radius in [0.6, 3.0, 20.0, 100.0] {
                let out = apply(&src, None, &Adjust::Blur { radius }, false, clip);
                assert!(pixels_in(clip).all(|(x, y)| at(&out, x, y) == colour), "{colour:?} 半径 {radius}");
                // 一个像素都没变，所以分块全部共用。
                assert!(shared(&out, &src), "{colour:?} 半径 {radius}");
            }
        }
    }

    #[test]
    fn blur_spreads_evenly_without_dark_fringes() {
        let clip = rect(0, 0, 400, 400);
        let mut src = TileMap::default();
        // 白色方块正好在画布中间，离画布边足够远。
        paint(&mut src, rect(185, 185, 215, 215), |_, _| [FULL; 4]);
        for radius in [1.0, 2.0, 9.0, 30.0] {
            let out = apply(&src, None, &Adjust::Blur { radius }, false, clip);
            assert_tidy(&out, clip);
            // 总的覆盖量守恒。
            let ratio = alpha_sum(&out, clip) / alpha_sum(&src, clip);
            assert!((ratio - 1.0).abs() < 0.01, "半径 {radius}：{ratio}");
            assert!(at(&out, 184, 200)[3] > 0 && at(&out, 200, 185)[3] < FULL, "半径 {radius}：没有模糊开");
            for (x, y) in pixels_in(clip) {
                let p = at(&out, x, y);
                // 去掉预乘之后仍是纯白：边缘不发灰、不发黑。
                assert!(p[0] == p[3] && p[1] == p[3] && p[2] == p[3], "半径 {radius} ({x}, {y})：{p:?}");
                // 左右、上下、沿对角线都对称（滚动求和的舍入差不出一个单位）。
                for q in [at(&out, 399 - x, y), at(&out, x, 399 - y), at(&out, y, x)] {
                    assert!(q[3].abs_diff(p[3]) <= 1, "半径 {radius} ({x}, {y})：{p:?} 对 {q:?}");
                }
            }
        }
    }

    #[test]
    fn blur_has_the_requested_spread() {
        // 一条贴满画布高度的竖线：纵向延伸边缘之后不变，横向的方差应当是 σ² =（半径 / 2）²。
        let clip = rect(0, 0, 640, 64);
        let mut src = TileMap::default();
        paint(&mut src, rect(320, 0, 321, 64), |_, _| [FULL; 4]);
        for radius in [0.5, 1.0, 2.5, 7.0, 20.0, 60.0] {
            let out = apply(&src, None, &Adjust::Blur { radius }, false, clip);
            let row: Vec<f64> = (0..640).map(|x| at(&out, x, 32)[3] as f64).collect();
            let total: f64 = row.iter().sum();
            let mean = row.iter().enumerate().map(|(x, w)| x as f64 * w).sum::<f64>() / total;
            let var = row.iter().enumerate().map(|(x, w)| (x as f64 - mean).powi(2) * w).sum::<f64>() / total;
            let want = (radius as f64 / 2.0).powi(2);
            assert!((total / ONE as f64 - 1.0).abs() < 0.005, "半径 {radius}：总量 {total}");
            assert!((mean - 320.0).abs() < 0.01, "半径 {radius}：中心 {mean}");
            assert!((var - want).abs() < 0.03 * want + 0.01, "半径 {radius}：方差 {var}，应为 {want}");
            assert!((0..640).all(|x| at(&out, x, 0) == at(&out, x, 32) && at(&out, x, 63) == at(&out, x, 32)), "半径 {radius}");
        }
    }

    #[test]
    fn blur_respects_the_canvas_edge() {
        let clip = rect(0, 0, 200, 150);
        // 贴着画布左边的一条竖条，和画布中间一样宽的一条；都从上边贴到下边。
        let mut src = TileMap::default();
        paint(&mut src, rect(0, 0, 20, 150), |_, _| [FULL; 4]);
        paint(&mut src, rect(100, 0, 120, 150), |_, _| [FULL; 4]);
        let out = apply(&src, None, &Adjust::Blur { radius: 5.0 }, false, clip);
        assert_tidy(&out, clip);
        // 画布边上延伸边缘像素：贴边那一侧不变淡，四个角上也一样。
        for (x, y) in [(0, 0), (0, 75), (0, 149), (10, 0)] {
            assert_eq!(at(&out, x, y), [FULL; 4], "({x}, {y})");
        }
        // 内容自己的边缘照常变淡，两条竖条的边一模一样。
        let edge = at(&out, 19, 75)[3];
        assert!(edge < FULL && edge > H / 2, "{edge}");
        assert!(at(&out, 100, 75)[3].abs_diff(edge) <= 1 && at(&out, 119, 75)[3].abs_diff(edge) <= 1);
        assert!(at(&out, 20, 75)[3] > 0 && at(&out, 99, 75)[3] > 0);

        // 画布外面的内容不参与，原样留着（实现方案 §3.15）。
        let spill = noise_in(rect(150, 100, 300, 260));
        for adjust in [Adjust::Blur { radius: 8.0 }, Adjust::HueSat { hue: 40.0, saturation: 0.0, lightness: 0.0 }] {
            let out = apply(&spill, None, &adjust, false, clip);
            let inside = |x: i32, y: i32| x < clip.x1 && y < clip.y1;
            assert!(pixels_in(rect(150, 100, 300, 260)).filter(|&(x, y)| !inside(x, y)).all(|(x, y)| at(&out, x, y) == at(&spill, x, y)));
            assert!(pixels_in(rect(150, 100, 200, 150)).any(|(x, y)| at(&out, x, y) != at(&spill, x, y)), "画布里的应当变了");
        }
        let mut outside = TileMap::default();
        paint(&mut outside, rect(200, 0, 300, 150), |_, _| [FULL; 4]);
        assert!(shared(&apply(&outside, None, &Adjust::Blur { radius: 30.0 }, false, clip), &outside));
        // 有选区时画布外面的像素不算选中，原样留着。
        let kept = apply(&spill, Some(&Selection::rect(rect(0, 0, 400, 400))), &Adjust::Blur { radius: 8.0 }, false, clip);
        assert!(pixels_in(rect(200, 100, 300, 260)).all(|(x, y)| at(&kept, x, y) == at(&spill, x, y)));
    }

    #[test]
    fn threads_do_not_change_the_result() {
        let clip = rect(0, 0, 300, 260);
        let src = noise_in(rect(-10, 5, 290, 270));
        let sel = half_selection(rect(20, 30, 280, 200), 150);
        let cases = [
            Adjust::HueSat { hue: 100.0, saturation: -0.3, lightness: 0.2 },
            Adjust::BrightContrast { brightness: 0.2, contrast: -0.3 },
            Adjust::Blur { radius: 3.5 },
            Adjust::Blur { radius: 25.0 },
        ];
        for adjust in cases {
            for s in [None, Some(&sel)] {
                for keep_alpha in [false, true] {
                    let one = run(&src, s, &adjust, keep_alpha, clip, 1);
                    for threads in [2, 3, 8] {
                        assert!(same(&run(&src, s, &adjust, keep_alpha, clip, threads), &one), "{adjust:?}，{threads} 个线程");
                    }
                }
            }
        }
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        let clip = rect(0, 0, 200, 150);
        let src = noise_in(rect(20, 20, 120, 90));
        let sel = Selection::rect(rect(40, 30, 90, 70));
        let odd = [
            Adjust::HueSat { hue: f32::INFINITY, saturation: f32::NAN, lightness: f32::NEG_INFINITY },
            Adjust::HueSat { hue: 1e-30, saturation: 0.0, lightness: 0.0 },
            Adjust::BrightContrast { brightness: f32::INFINITY, contrast: f32::INFINITY },
            Adjust::BrightContrast { brightness: f32::NEG_INFINITY, contrast: f32::NAN },
            Adjust::Blur { radius: f32::INFINITY },
            Adjust::Blur { radius: 1e30 },
            Adjust::Blur { radius: 1e-30 },
        ];
        for adjust in odd {
            for s in [None, Some(&sel), Some(&Selection::empty())] {
                for keep_alpha in [false, true] {
                    let out = apply(&src, s, &adjust, keep_alpha, clip);
                    if s.is_none() {
                        assert_tidy(&out, clip);
                    }
                }
            }
        }
        // 超过 100 的半径按 100 算。
        assert!(same(
            &apply(&src, None, &Adjust::Blur { radius: 250.0 }, false, clip),
            &apply(&src, None, &Adjust::Blur { radius: 100.0 }, false, clip)
        ));
        // clip 是空的、或者和内容不沾边：什么都不改；图层是空的。
        for c in [Rect::EMPTY, rect(500, 500, 600, 600), rect(10, 10, 5, 5)] {
            assert!(shared(&apply(&src, None, &Adjust::Blur { radius: 5.0 }, false, c), &src));
            assert!(shared(&apply(&src, None, &Adjust::HueSat { hue: 30.0, saturation: 0.0, lightness: 0.0 }, false, c), &src));
            assert!(shared(&apply(&src, Some(&sel), &Adjust::Blur { radius: 5.0 }, false, c), &src));
        }
        assert!(apply(&TileMap::default(), None, &Adjust::Blur { radius: 10.0 }, false, clip).is_empty());
        // 比模糊半径还小的画布。
        let tiny = rect(0, 0, 3, 2);
        assert_tidy(&apply(&noise_in(tiny), None, &Adjust::Blur { radius: 100.0 }, false, tiny), tiny);
        // 长度不对的分块当作不存在。
        let mut broken = src.clone();
        broken.replace((0, 0), Some(Arc::new(Tile(vec![FULL; 7]))));
        assert_tidy(&apply(&broken, None, &Adjust::Blur { radius: 3.0 }, false, clip), clip);
        assert_tidy(&apply(&broken, None, &Adjust::HueSat { hue: 30.0, saturation: 0.0, lightness: 0.0 }, false, clip), clip);
        apply(&broken, Some(&sel), &Adjust::Blur { radius: 3.0 }, true, clip);
    }

    /// 性能测试用的画面：3000×2000 的画布铺满，不透明和半透明的像素都有，相邻像素各不相同。
    fn big_picture() -> (TileMap, Rect) {
        let clip = rect(0, 0, 3000, 2000);
        let mut src = TileMap::default();
        paint(&mut src, clip, |x, y| {
            let a = if (x / 37 + y / 53) % 4 == 0 { ONE / 3 + (x * 7 + y * 13) as u32 % (ONE / 2) } else { ONE };
            [(x as u32 * 16 % (a + 1)) as u16, (y as u32 * 11 % (a + 1)) as u16, (a / 2) as u16, a as u16]
        });
        (src, clip)
    }

    /// 做一次滤镜的毫秒数。取三次里最快的，免得被同时在跑的别的任务拖累。
    fn millis(src: &TileMap, adjust: &Adjust, clip: Rect) -> f64 {
        let once = |_| {
            let t = Instant::now();
            let out = apply(src, None, adjust, false, clip);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(!out.is_empty());
            ms
        };
        (0..3).map(once).fold(f64::MAX, f64::min)
    }

    /// 各种滤镜的耗时，手动跑着看：
    /// `cargo test -p zhibi-core --release filter::tests::timings -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn timings() {
        let (src, clip) = big_picture();
        let cases = [
            ("色相/饱和度/明度 30°、0.2、0.1", Adjust::HueSat { hue: 30.0, saturation: 0.2, lightness: 0.1 }),
            ("亮度/对比度 0.2、0.3", Adjust::BrightContrast { brightness: 0.2, contrast: 0.3 }),
            ("模糊 5", Adjust::Blur { radius: 5.0 }),
            ("模糊 30", Adjust::Blur { radius: 30.0 }),
            ("模糊 100", Adjust::Blur { radius: 100.0 }),
        ];
        let profile = if cfg!(debug_assertions) { "dev" } else { "release" };
        for (name, adjust) in cases {
            eprintln!("[{profile}] {name}：{:.2} ms", millis(&src, &adjust, clip));
        }
    }
}
