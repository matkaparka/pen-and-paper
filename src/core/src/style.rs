// SPDX-License-Identifier: GPL-3.0-or-later
//! 图层样式（规格 F-062，实现方案 §3.14）：投影、外发光、内发光三种效果的参数和画法。
//!
//! 效果只看图层的形状（像素的透明度乘蒙版）：平移、扩张、模糊，得到每个效果自己的一张透明度图，
//! 合成时用效果的颜色、不透明度和混合模式叠上去（投影、外发光在图层下面，内发光在上面）。
//! 一个图层的效果图要对整层做几次模糊，所以按图层缓存：像素、蒙版或效果变了才重算。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::document::{BlendMode, Mask};
use crate::tile::{Tile, TileCoord, TileMap, TILE};
use crate::{Rect, ONE};

const N: usize = (TILE * TILE) as usize;

/// 一种效果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EffectKind {
    /// 投影：往 (dx, dy) 落（像素，已乘效果缩放）；`knockout` 是"图层挖空投影"
    DropShadow { dx: f32, dy: f32, knockout: bool },
    OuterGlow,
    /// 内发光：`center` 为真时从中心发，否则从边缘发
    InnerGlow { center: bool },
}

/// 一个效果的参数（读文件时已乘上效果缩放）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Effect {
    pub kind: EffectKind,
    pub color: [u8; 3],
    /// 不透明度，0..=1
    pub opacity: f32,
    pub blend: BlendMode,
    /// 扩展（投影、外发光）或阻塞（内发光），0..=1
    pub spread: f32,
    /// 大小，像素
    pub size: f32,
    /// 范围（发光才有），0..=1
    pub range: f32,
}

impl Effect {
    /// 叠在图层下面（投影、外发光），还是上面（内发光）。
    pub fn below(&self) -> bool {
        !matches!(self.kind, EffectKind::InnerGlow { .. })
    }
}

/// 一个图层的样式。
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    /// PS 原来的块（`lfx2`，有旧版的 `lrFX` 也在），存回 PSD 时原样写回去
    pub psd: Vec<([u8; 4], Vec<u8>)>,
    /// 本程序会画的效果
    pub effects: Vec<Effect>,
    /// 总开关
    pub enabled: bool,
    /// `lfx2` 里总开关那个字节：（块在 `psd` 里的序号，字节位置）
    pub switch_at: Option<(usize, usize)>,
    /// 文件里的全局光（角度、高度，度）。用全局光的效果靠它定方向；存回 PSD 时写回图像资源里
    pub light: Option<(i32, i32)>,
}

impl Style {
    /// 合成时要不要画效果。
    pub fn shows(&self) -> bool {
        self.enabled && !self.effects.is_empty()
    }

    /// 打开或关掉效果，PSD 原始块里的总开关一起改，存回 PSD 时 PS 里也是这样。
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if let Some((b, at)) = self.switch_at {
            if let Some(byte) = self.psd.get_mut(b).and_then(|(_, d)| d.get_mut(at)) {
                *byte = on as u8;
            }
        }
    }
}

/// 画法里 PS 没有公开、要拿 PS 的结果校准的几个数（实现方案 §3.14）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Model {
    /// 高斯的标准差是模糊长度（大小减去扩展的部分）的几倍
    pub sigma_per_size: f32,
    /// 用几次方框模糊逼近（次数越少，轮廓越像方框、尾巴越短）
    pub passes: usize,
    /// 内发光的标准差另算：是模糊长度的几倍
    pub sigma_inner: f32,
    /// 发光的"范围"怎么作用在透明度上
    pub range: RangeRule,
}

/// "范围"的几种可能的作用（a 是模糊后的透明度，r 是范围）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RangeRule {
    /// 不管范围
    Ignore,
    /// a / r，到 1 为止
    Scale,
    /// (a − (1 − r)) / r，夹在 0..1
    Shift,
}

/// 校准后定下来的画法。
pub const PS_MODEL: Model = Model { sigma_per_size: 0.2, passes: 3, sigma_inner: 0.2, range: RangeRule::Scale };

/// 当前用的画法：平时就是 `PS_MODEL`；校准的测试会临时换掉（换了以后缓存自动作废）。
static MODEL: RwLock<(Model, u64)> = RwLock::new((PS_MODEL, 0));

pub fn model() -> (Model, u64) {
    *MODEL.read().unwrap_or_else(|e| e.into_inner())
}

/// 换画法（只给校准用）。
pub fn set_model(m: Model) {
    let mut g = MODEL.write().unwrap_or_else(|e| e.into_inner());
    *g = (m, g.1 + 1);
}

/// 一个图层所有效果画好的透明度（0..=ONE，还没乘效果的不透明度），按分块存，只有画布里的；
/// 和 `effects` 一一对应。
#[derive(Clone)]
pub struct Planes {
    pub fx: Vec<HashMap<TileCoord, Arc<Vec<u16>>>>,
}

/// 效果图怎么算的：这些一样，图层内容变了时才能只重算变了的那一片。
#[derive(Clone, PartialEq)]
struct Key {
    mask: Option<(u16, bool)>,
    effects: Vec<Effect>,
    canvas: Rect,
    model: u64,
}

/// 缓存里的一项。
struct Entry {
    key: Key,
    /// 算的时候分块表和蒙版的改动编号；都没变就直接用
    revisions: (u64, u64),
    /// 算的时候的各个分块。拿着它们，图层上再改这些分块时就会先复制一份（写时复制），
    /// 和这里拿着的不再是同一块，对一对就知道哪些分块变了
    tiles: HashMap<TileCoord, Arc<Tile>>,
    mask: HashMap<TileCoord, Arc<Vec<u16>>>,
    planes: Arc<Planes>,
}

/// 按图层存的效果缓存。复制图层时一起复制（共用同一份，键对不上会自己重算）。
#[derive(Clone, Default)]
pub struct Cache(Arc<Mutex<Option<Entry>>>);

impl Cache {
    /// 这个图层的效果图：和上次算的时候一样就直接用；只是一部分分块变了（画了一笔），就只重算
    /// 那几块够得着的地方；别的情况整层重算。几个线程同时要时只算一次。
    pub fn get(&self, tiles: &TileMap, mask: Option<&Mask>, effects: &[Effect], canvas: Rect) -> Arc<Planes> {
        let (m, rev) = model();
        let mask = mask.filter(|m| !m.disabled);
        let key = Key { mask: mask.map(|m| (m.default, m.disabled)), effects: effects.to_vec(), canvas, model: rev };
        let revisions = (tiles.revision(), mask.map_or(0, |m| m.tiles.revision()));
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now: HashMap<TileCoord, Arc<Tile>> = tiles.coords().map(|tc| (tc, tiles.get(tc).unwrap().clone())).collect();
        let now_mask: HashMap<TileCoord, Arc<Vec<u16>>> =
            mask.map(|m| m.tiles.coords().map(|tc| (tc, m.tiles.get(tc).unwrap().clone())).collect()).unwrap_or_default();
        if let Some(e) = g.as_mut().filter(|e| e.key == key) {
            if e.revisions == revisions {
                return e.planes.clone();
            }
            // 哪些分块换了：地址不一样、多了、少了。
            let changed: Vec<TileCoord> = differs(&e.tiles, &now).chain(differs(&e.mask, &now_mask)).collect();
            if changed.len() * 4 <= now.len().max(e.tiles.len()).max(16) {
                if !changed.is_empty() {
                    let mut planes = (*e.planes).clone();
                    let area = changed.iter().fold(Rect::EMPTY, |r, &tc| r.union(tile_rect(tc)));
                    patch(&mut planes, tiles, mask, effects, canvas, &m, area);
                    e.planes = Arc::new(planes);
                }
                e.revisions = revisions;
                e.tiles = now;
                e.mask = now_mask;
                return e.planes.clone();
            }
        }
        let planes = Arc::new(render(tiles, mask, effects, canvas, &m));
        *g = Some(Entry { key, revisions, tiles: now, mask: now_mask, planes: planes.clone() });
        planes
    }
}

/// 两份分块表里不是同一块的坐标（多了的、少了的也算）。
fn differs<'a, T>(old: &'a HashMap<TileCoord, Arc<T>>, new: &'a HashMap<TileCoord, Arc<T>>) -> impl Iterator<Item = TileCoord> + 'a {
    old.iter()
        .filter(move |(tc, a)| new.get(tc).is_none_or(|b| !Arc::ptr_eq(a, b)))
        .map(|(tc, _)| *tc)
        .chain(new.keys().filter(move |tc| !old.contains_key(tc)).copied())
}

/// 形状在 `changed` 里变了：重算效果够得着的那一片（对齐到分块），换进 `planes`。
/// 那一片外面的结果只取决于没变的形状，不用动。
fn patch(planes: &mut Planes, tiles: &TileMap, mask: Option<&Mask>, effects: &[Effect], canvas: Rect, model: &Model, changed: Rect) {
    let reach = effects.iter().map(|e| reach_of(e, model)).max().unwrap_or(0);
    let near = grow(canvas, reach);
    // 要重算的分块：变了的地方四周扩出效果能够到的距离，对齐到分块，只要画布里的。
    let out = align(grow(changed, reach)).intersect(canvas);
    if out.is_empty() {
        return;
    }
    let coords: Vec<TileCoord> = crate::tile::tiles_in(out).collect();
    let area = grow(align(out), reach).intersect(near);
    let shape = shape(tiles, mask, area);
    for (e, fx) in effects.iter().zip(&mut planes.fx) {
        let plane = effect_plane(e, &shape, model);
        for &tc in &coords {
            match tile_values(&plane, tc, canvas) {
                Some(t) => fx.insert(tc, Arc::new(t)),
                None => fx.remove(&tc),
            };
        }
    }
}

/// 扩到分块边界。
fn align(r: Rect) -> Rect {
    if r.is_empty() {
        return r;
    }
    Rect {
        x0: r.x0.div_euclid(TILE) * TILE,
        y0: r.y0.div_euclid(TILE) * TILE,
        x1: (r.x1 - 1).div_euclid(TILE) * TILE + TILE,
        y1: (r.y1 - 1).div_euclid(TILE) * TILE + TILE,
    }
}

// ---- 画法 ----

/// 一块浮点的透明度图，覆盖矩形 `area`。
struct Plane {
    area: Rect,
    w: usize,
    v: Vec<f32>,
}

impl Plane {
    fn new(area: Rect, fill: f32) -> Self {
        let (w, h) = ((area.x1 - area.x0) as usize, (area.y1 - area.y0) as usize);
        Plane { area, w, v: vec![fill; w * h] }
    }

    fn h(&self) -> usize {
        self.v.len().checked_div(self.w).unwrap_or(0)
    }

    /// (x, y) 处的值，图外是 `outside`。
    fn at(&self, x: i32, y: i32, outside: f32) -> f32 {
        let a = self.area;
        if x < a.x0 || y < a.y0 || x >= a.x1 || y >= a.y1 {
            return outside;
        }
        self.v[(y - a.y0) as usize * self.w + (x - a.x0) as usize]
    }
}

/// 画出一个图层的全部效果。`canvas` 是画布：只有画布里的结果有用，但模糊要用到画布外的形状。
pub fn render(tiles: &TileMap, mask: Option<&Mask>, effects: &[Effect], canvas: Rect, model: &Model) -> Planes {
    let reach = effects.iter().map(|e| reach_of(e, model)).max().unwrap_or(0);
    let content = tiles.coords().fold(Rect::EMPTY, |r, tc| r.union(tile_rect(tc)));
    let near = grow(canvas, reach);
    let area = grow(content.intersect(near), reach).intersect(near);
    let mut out = Planes { fx: effects.iter().map(|_| HashMap::new()).collect() };
    if area.is_empty() {
        return out;
    }
    let shape = shape(tiles, mask, area);
    for (e, fx) in effects.iter().zip(&mut out.fx) {
        let plane = effect_plane(e, &shape, model);
        *fx = to_tiles(&plane, canvas);
    }
    out
}

/// 一个效果最远能够到形状外面多远（像素）：平移、扩张、模糊各占一段，模糊按三个标准差算。
fn reach_of(e: &Effect, model: &Model) -> i32 {
    let (grow_px, sigma) = split(e, model);
    let shift = match e.kind {
        EffectKind::DropShadow { dx, dy, .. } => dx.abs().max(dy.abs()),
        _ => 0.0,
    };
    (shift + grow_px + 3.0 * sigma).ceil() as i32 + 2
}

/// 大小分成两段：扩展（阻塞）的那部分是硬扩张，剩下的是模糊。返回（扩张的像素，高斯的标准差）。
fn split(e: &Effect, model: &Model) -> (f32, f32) {
    let size = e.size.max(0.0);
    let spread = e.spread.clamp(0.0, 1.0);
    let k = if e.below() { model.sigma_per_size } else { model.sigma_inner };
    (size * spread, size * (1.0 - spread) * k)
}

fn effect_plane(e: &Effect, shape: &Plane, model: &Model) -> Plane {
    let (grow_px, sigma) = split(e, model);
    match e.kind {
        EffectKind::DropShadow { dx, dy, knockout } => {
            let mut p = shifted(shape, dx, dy);
            dilate(&mut p, grow_px);
            blur_with(&mut p, sigma, 0.0, model.passes);
            if knockout {
                for (v, a) in p.v.iter_mut().zip(&shape.v) {
                    *v *= 1.0 - a;
                }
            }
            p
        }
        EffectKind::OuterGlow => {
            let mut p = Plane { area: shape.area, w: shape.w, v: shape.v.clone() };
            dilate(&mut p, grow_px);
            blur_with(&mut p, sigma, 0.0, model.passes);
            apply_range(&mut p, e.range, model.range);
            p
        }
        EffectKind::InnerGlow { center } => {
            // 从边缘发：形状外面（1 − A）扩张、模糊，伸进形状里的部分就是发光。
            let mut p = Plane { area: shape.area, w: shape.w, v: shape.v.iter().map(|a| 1.0 - a).collect() };
            dilate(&mut p, grow_px);
            blur_with(&mut p, sigma, 1.0, model.passes);
            apply_range(&mut p, e.range, model.range);
            for (v, a) in p.v.iter_mut().zip(&shape.v) {
                *v = if center { (1.0 - *v) * a } else { *v * a };
            }
            p
        }
    }
}

fn apply_range(p: &mut Plane, range: f32, rule: RangeRule) {
    let r = range.clamp(0.01, 1.0);
    for v in &mut p.v {
        *v = match rule {
            RangeRule::Ignore => *v,
            RangeRule::Scale => (*v / r).min(1.0),
            RangeRule::Shift => ((*v - (1.0 - r)) / r).clamp(0.0, 1.0),
        };
    }
}

/// 图层的形状：像素的透明度乘蒙版，0..=1。
fn shape(tiles: &TileMap, mask: Option<&Mask>, area: Rect) -> Plane {
    let mut p = Plane::new(area, 0.0);
    let one = ONE as f32;
    for tc in tiles.coords() {
        let r = tile_rect(tc).intersect(area);
        if r.is_empty() {
            continue;
        }
        let t = tiles.get(tc).unwrap();
        if t.0.len() != N * 4 {
            continue;
        }
        let m = mask.map(|m| (m.tile(tc), m.default));
        for y in r.y0..r.y1 {
            for x in r.x0..r.x1 {
                let k = ((y - tc.1 * TILE) * TILE + (x - tc.0 * TILE)) as usize;
                let mut a = t.0[k * 4 + 3] as f32 / one;
                if let Some((tile, default)) = m {
                    a *= tile.map_or(default, |t| t[k]) as f32 / one;
                }
                p.v[(y - area.y0) as usize * p.w + (x - area.x0) as usize] = a.min(1.0);
            }
        }
    }
    p
}

/// 平移 (dx, dy)，按双线性取样。
fn shifted(p: &Plane, dx: f32, dy: f32) -> Plane {
    let mut out = Plane::new(p.area, 0.0);
    let (fx, fy) = (dx.floor(), dy.floor());
    let (tx, ty) = (dx - fx, dy - fy);
    let (ix, iy) = (fx as i32, fy as i32);
    let a = p.area;
    for y in a.y0..a.y1 {
        for x in a.x0..a.x1 {
            // 源点 (x − dx, y − dy) 落在 (sx, sy) 和它右下的三个点之间。
            let (sx, sy) = (x - ix - 1, y - iy - 1);
            let v00 = p.at(sx, sy, 0.0);
            let v10 = p.at(sx + 1, sy, 0.0);
            let v01 = p.at(sx, sy + 1, 0.0);
            let v11 = p.at(sx + 1, sy + 1, 0.0);
            // 权重：离 (x − dx) 越近越大。
            let (wx, wy) = (1.0 - tx, 1.0 - ty);
            let v = v00 * (1.0 - wx) * (1.0 - wy) + v10 * wx * (1.0 - wy) + v01 * (1.0 - wx) * wy + v11 * wx * wy;
            out.v[(y - a.y0) as usize * out.w + (x - a.x0) as usize] = v;
        }
    }
    out
}

/// 灰度扩张（取邻域里的最大值），方形的邻域，半径四舍五入到整像素。逐行、逐列各做一遍，
/// 用分段的前缀、后缀最大值，每个点的代价和半径无关。
fn dilate(p: &mut Plane, r: f32) {
    let r = r.round() as usize;
    if r == 0 {
        return;
    }
    separable(p, |line, out| max_filter(line, r, out));
}

/// 对每一行、再对每一列做同一个一维的运算（`f(这一行, 结果)`）。行分给几个线程；
/// 列的那一遍先把图转置，当成行来做，再转回来。
fn separable(p: &mut Plane, f: impl Fn(&[f32], &mut Vec<f32>) + Sync) {
    let (w, h) = (p.w, p.h());
    if w == 0 || h == 0 {
        return;
    }
    rows(&mut p.v, w, &f);
    let mut t = transpose(&p.v, w, h);
    rows(&mut t, h, &f);
    p.v = transpose(&t, h, w);
}

/// 宽 `w` 的图，每一行换成 `f` 算出来的结果，几个线程分着做。
fn rows(v: &mut [f32], w: usize, f: &(impl Fn(&[f32], &mut Vec<f32>) + Sync)) {
    let h = v.len() / w;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let per = h.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for chunk in v.chunks_mut(per * w) {
            s.spawn(move || {
                let (mut line, mut out) = (Vec::with_capacity(w), Vec::with_capacity(w));
                for row in chunk.chunks_mut(w) {
                    line.clear();
                    line.extend_from_slice(row);
                    f(&line, &mut out);
                    row.copy_from_slice(&out);
                }
            });
        }
    });
}

/// 宽 `w`、高 `h` 的图转置成宽 `h`、高 `w` 的。分块转，缓存友好些。
fn transpose(v: &[f32], w: usize, h: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; w * h];
    const B: usize = 32;
    for by in (0..h).step_by(B) {
        for bx in (0..w).step_by(B) {
            for y in by..(by + B).min(h) {
                for x in bx..(bx + B).min(w) {
                    out[x * h + y] = v[y * w + x];
                }
            }
        }
    }
    out
}

/// 一维的滑动最大值，窗口 [i − r, i + r]，两头外面当 0（形状外面没有东西）。
fn max_filter(src: &[f32], r: usize, out: &mut Vec<f32>) {
    let n = src.len();
    let k = 2 * r + 1;
    // 两头各补 r 个 0，窗口就总在数组里。
    let padded: Vec<f32> = std::iter::repeat_n(0.0, r).chain(src.iter().copied()).chain(std::iter::repeat_n(0.0, r)).collect();
    let m = padded.len();
    let mut pre = vec![0.0f32; m];
    let mut suf = vec![0.0f32; m];
    for i in 0..m {
        pre[i] = if i % k == 0 { padded[i] } else { pre[i - 1].max(padded[i]) };
    }
    for i in (0..m).rev() {
        suf[i] = if i == m - 1 || (i + 1) % k == 0 { padded[i] } else { suf[i + 1].max(padded[i]) };
    }
    out.clear();
    out.extend((0..n).map(|i| suf[i].max(pre[i + k - 1])));
}

/// 近似高斯模糊：`passes` 次方框模糊，图外的值是 `outside`。
fn blur_with(p: &mut Plane, sigma: f32, outside: f32, passes: usize) {
    if sigma < 0.3 {
        return;
    }
    // 几个方框先都沿行做、再都沿列做：两个方向的运算互不相干，先后不影响结果。
    let radii = box_radii(sigma, passes);
    separable(p, |line, out| {
        let mut a = line.to_vec();
        for &r in &radii {
            box_filter(&a, r, outside, out);
            std::mem::swap(&mut a, out);
        }
        std::mem::swap(&mut a, out);
    });
}

/// `count` 次方框模糊合起来的标准差是 σ 时，各个方框的半径（宽度是 2r + 1）。
fn box_radii(sigma: f32, count: usize) -> Vec<usize> {
    let count = count.max(1);
    let n = count as f32;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = ideal.floor() as i64;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wu = wl + 2;
    let m = ((12.0 * sigma * sigma - n * (wl * wl) as f32 - 4.0 * n * wl as f32 - 3.0 * n) / (-4.0 * wl as f32 - 4.0)).round() as i64;
    let mut out = vec![0usize; count];
    for (i, r) in out.iter_mut().enumerate() {
        let width = if (i as i64) < m { wl } else { wu };
        *r = ((width.max(1) - 1) / 2) as usize;
    }
    out
}

/// 一维方框平均，窗口 [i − r, i + r]，两头外面的值是 `outside`。
fn box_filter(src: &[f32], r: usize, outside: f32, out: &mut Vec<f32>) {
    let n = src.len() as i64;
    let r = r as i64;
    let get = |i: i64| if i < 0 || i >= n { outside } else { src[i as usize] };
    let k = (2 * r + 1) as f32;
    let mut sum: f64 = (-r..=r).map(|i| get(i) as f64).sum();
    out.clear();
    for i in 0..n {
        out.push((sum / k as f64) as f32);
        sum += get(i + r + 1) as f64 - get(i - r) as f64;
    }
}

/// 只留画布里的分块，0..=1 换成 0..=ONE；全是 0 的分块不存。
fn to_tiles(p: &Plane, canvas: Rect) -> HashMap<TileCoord, Arc<Vec<u16>>> {
    crate::tile::tiles_in(p.area.intersect(canvas)).filter_map(|tc| tile_values(p, tc, canvas).map(|t| (tc, Arc::new(t)))).collect()
}

/// 分块 tc 在画布里的那部分的值（0..=ONE）；全是 0 时是 None。
fn tile_values(p: &Plane, tc: TileCoord, canvas: Rect) -> Option<Vec<u16>> {
    let r = tile_rect(tc).intersect(canvas).intersect(p.area);
    let mut t = vec![0u16; N];
    let mut any = false;
    for y in r.y0..r.y1 {
        for x in r.x0..r.x1 {
            let v = (p.at(x, y, 0.0).clamp(0.0, 1.0) * ONE as f32).round() as u16;
            any |= v != 0;
            t[((y - tc.1 * TILE) * TILE + (x - tc.0 * TILE)) as usize] = v;
        }
    }
    any.then_some(t)
}

fn tile_rect(tc: TileCoord) -> Rect {
    Rect { x0: tc.0 * TILE, y0: tc.1 * TILE, x1: (tc.0 + 1) * TILE, y1: (tc.1 + 1) * TILE }
}

fn grow(r: Rect, by: i32) -> Rect {
    if r.is_empty() {
        return r;
    }
    Rect { x0: r.x0 - by, y0: r.y0 - by, x1: r.x1 + by, y1: r.y1 + by }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::Tile;

    fn square(r: Rect) -> TileMap {
        let mut t = TileMap::default();
        for y in r.y0..r.y1 {
            for x in r.x0..r.x1 {
                let tile = t.get_mut((x.div_euclid(TILE), y.div_euclid(TILE)));
                let k = ((y.rem_euclid(TILE)) * TILE + x.rem_euclid(TILE)) as usize * 4;
                tile.0[k..k + 4].copy_from_slice(&[ONE as u16; 4]);
            }
        }
        t
    }

    fn value(p: &Planes, i: usize, x: i32, y: i32) -> u16 {
        p.fx[i].get(&(x.div_euclid(TILE), y.div_euclid(TILE))).map_or(0, |t| t[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize])
    }

    fn effect(kind: EffectKind, spread: f32, size: f32) -> Effect {
        Effect { kind, color: [0; 3], opacity: 1.0, blend: BlendMode::Normal, spread, size, range: 1.0 }
    }

    #[test]
    fn box_blur_keeps_the_total_and_matches_the_spread() {
        // 一个点（一维）模糊之后总量不变，方差接近 σ²（方框宽度是整数，σ 很小时粗一些，效果的大小一般都在几个像素以上）。
        for sigma in [3.0f32, 9.0, 40.0] {
            let n = 400;
            let mut line = vec![0.0f32; n];
            line[n / 2] = 1.0;
            let mut out = Vec::new();
            for r in box_radii(sigma, 3) {
                box_filter(&line, r, 0.0, &mut out);
                std::mem::swap(&mut line, &mut out);
            }
            let total: f32 = line.iter().sum();
            let var: f32 = line.iter().enumerate().map(|(i, v)| v * (i as f32 - (n / 2) as f32).powi(2)).sum::<f32>() / total;
            assert!((total - 1.0).abs() < 1e-3, "{total}");
            assert!((var.sqrt() - sigma).abs() / sigma < 0.1, "σ {sigma}：{}", var.sqrt());
        }
    }

    #[test]
    fn max_filter_is_a_sliding_maximum() {
        let src = [0.0, 0.2, 0.9, 0.1, 0.0, 0.0, 0.5, 0.0];
        let mut out = Vec::new();
        max_filter(&src, 1, &mut out);
        assert_eq!(out, [0.2, 0.9, 0.9, 0.9, 0.1, 0.5, 0.5, 0.5]);
        max_filter(&src, 3, &mut out);
        let slow: Vec<f32> = (0..src.len()).map(|i| src[i.saturating_sub(3)..(i + 4).min(src.len())].iter().copied().fold(0.0, f32::max)).collect();
        assert_eq!(out, slow);
    }

    #[test]
    fn shadow_falls_where_it_should_and_is_knocked_out_under_the_layer() {
        let tiles = square(Rect { x0: 40, y0: 40, x1: 80, y1: 80 });
        let canvas = Rect { x0: 0, y0: 0, x1: 200, y1: 200 };
        let fx = [effect(EffectKind::DropShadow { dx: 0.0, dy: 20.0, knockout: true }, 0.0, 0.0), effect(EffectKind::DropShadow { dx: 0.0, dy: 20.0, knockout: false }, 0.0, 0.0)];
        let p = render(&tiles, None, &fx, canvas, &PS_MODEL);
        // 不模糊：投影就是往下挪 20 的方块；挖空的那个在图层底下是 0。
        assert_eq!(value(&p, 0, 60, 95), ONE as u16);
        assert_eq!(value(&p, 0, 60, 70), 0);
        assert_eq!(value(&p, 1, 60, 70), ONE as u16);
        assert_eq!(value(&p, 0, 60, 105), 0);
    }

    #[test]
    fn glows_spread_outside_and_inside_the_shape() {
        let tiles = square(Rect { x0: 60, y0: 60, x1: 140, y1: 140 });
        let canvas = Rect { x0: 0, y0: 0, x1: 200, y1: 200 };
        let fx = [effect(EffectKind::OuterGlow, 0.0, 20.0), effect(EffectKind::InnerGlow { center: false }, 0.0, 20.0)];
        let p = render(&tiles, None, &fx, canvas, &PS_MODEL);
        // 外发光：形状边上最亮，往外越来越淡；内发光只在形状里面，边上最亮，往里越来越淡。
        let (o0, o1, o2) = (value(&p, 0, 59, 100), value(&p, 0, 55, 100), value(&p, 0, 52, 100));
        assert!(o0 > o1 && o1 > o2 && o2 > 0, "{o0} {o1} {o2}");
        assert_eq!(value(&p, 0, 30, 100), 0, "离得远了没有");
        let (i0, i1, i2) = (value(&p, 1, 60, 100), value(&p, 1, 64, 100), value(&p, 1, 100, 100));
        assert!(i0 > i1 && i1 > i2, "{i0} {i1} {i2}");
        assert_eq!(value(&p, 1, 59, 100), 0);
        // 扩展：大小一样，扩展 50% 的发光在边外更实。
        let wide = render(&tiles, None, &[effect(EffectKind::OuterGlow, 0.5, 20.0)], canvas, &PS_MODEL);
        assert!(value(&wide, 0, 52, 100) > value(&p, 0, 52, 100));
    }

    /// 同一个形状，算的范围大小不同，结果一样（只重算一片时靠这一条）。
    #[test]
    fn area_does_not_change_the_result() {
        let shape_tiles = square(Rect { x0: 40, y0: 40, x1: 200, y1: 160 });
        let m = model().0;
        for e in [effect(EffectKind::OuterGlow, 0.1, 12.0), effect(EffectKind::OuterGlow, 0.0, 12.0), effect(EffectKind::OuterGlow, 0.1, 0.0)] {
            let small = Rect { x0: 0, y0: 0, x1: 260, y1: 200 };
            let big = Rect { x0: -50, y0: -70, x1: 330, y1: 260 };
            let (p1, p2) = (effect_plane(&e, &shape(&shape_tiles, None, small), &m), effect_plane(&e, &shape(&shape_tiles, None, big), &m));
            let worst = (40..220).flat_map(|y| (20..230).map(move |x| (x, y))).map(|(x, y)| (p1.at(x, y, 0.0) - p2.at(x, y, 0.0)).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-5, "{e:?}：{worst}");
        }
    }

    /// 改了一部分分块之后只重算那一片：结果和整层重算一模一样。
    #[test]
    fn patching_after_a_stroke_matches_a_full_render() {
        let mut tiles = square(Rect { x0: 40, y0: 40, x1: 200, y1: 160 });
        let canvas = Rect { x0: 0, y0: 0, x1: 400, y1: 300 };
        let fx = [
            effect(EffectKind::OuterGlow, 0.1, 12.0),
            effect(EffectKind::InnerGlow { center: false }, 0.2, 30.0),
            effect(EffectKind::DropShadow { dx: 5.5, dy: 9.0, knockout: true }, 0.0, 8.0),
        ];
        let cache = Cache::default();
        let before = cache.get(&tiles, None, &fx, canvas);
        // "画一笔"：右下角添一小块，再擦掉左上角一块。
        for y in 150..190 {
            for x in 190..230 {
                let t = tiles.get_mut((x / TILE, y / TILE));
                let k = ((y % TILE) * TILE + x % TILE) as usize * 4;
                t.0[k..k + 4].copy_from_slice(&[ONE as u16; 4]);
            }
        }
        tiles.replace((0, 0), None);
        let patched = cache.get(&tiles, None, &fx, canvas);
        assert!(!Arc::ptr_eq(&before, &patched));
        let full = render(&tiles, None, &fx, canvas, &model().0);
        for (i, (a, b)) in patched.fx.iter().zip(&full.fx).enumerate() {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for tc in keys {
                let (x, y) = (a.get(tc).map(|t| t.as_slice()), b.get(tc).map(|t| t.as_slice()));
                if x != y {
                    let zero = vec![0u16; N];
                    let (x, y) = (x.unwrap_or(&zero), y.unwrap_or(&zero));
                    let k = (0..N).find(|&k| x[k] != y[k]).unwrap();
                    let n = (0..N).filter(|&k| x[k] != y[k]).count();
                    panic!("效果 {i} 分块 {tc:?}：{n} 个不同，第一个在 ({}, {})：分块重算 {} 整层 {}", tc.0 * TILE + k as i32 % TILE, tc.1 * TILE + k as i32 / TILE, x[k], y[k]);
                }
            }
        }
    }

    #[test]
    fn the_cache_recomputes_only_after_a_change() {
        let mut tiles = square(Rect { x0: 10, y0: 10, x1: 30, y1: 30 });
        let canvas = Rect { x0: 0, y0: 0, x1: 100, y1: 100 };
        let fx = [effect(EffectKind::OuterGlow, 0.0, 6.0)];
        let cache = Cache::default();
        let a = cache.get(&tiles, None, &fx, canvas);
        let b = cache.get(&tiles, None, &fx, canvas);
        assert!(Arc::ptr_eq(&a, &b));
        tiles.replace((5, 5), Some(Arc::new(Tile::new())));
        let c = cache.get(&tiles, None, &fx, canvas);
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
