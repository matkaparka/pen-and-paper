// SPDX-License-Identifier: GPL-3.0-or-later
//! 液化（规格 F-083，实现方案 §3.9）：推、转、收、鼓、复原。
//!
//! 开始液化时存下原图，之后只改一张位移场 D：画面上 p 处的颜色取自原图的 p + D(p)。
//! 画面每次都从原图重算，所以画多少笔都不会越画越糊；D 为零的像素原样照搬原图，
//! 所以复原到底时一位不差。重采样走的是 `transform` 里一般映射的那条路。

use std::collections::{HashMap, HashSet};
use std::f32::consts::{LN_2, PI};
use std::sync::Arc;

use crate::document::Document;
use crate::selection::Selection;
use crate::tile::{tiles_in, Tile, TileCoord, TileMap, TILE};
use crate::transform::{self, Mapping, Quality};
use crate::{Rect, ONE};

const N: usize = (TILE * TILE) as usize;
/// 分块边长是 2 的幂，由坐标取分块号和块内偏移时用移位和掩码（负数也对）。
const SHIFT: i32 = TILE.trailing_zeros() as i32;
const MASK: i32 = TILE - 1;

/// 一个分块位置和它的新内容（None 是全透明）。
pub type TileSlot = (TileCoord, Option<Arc<Tile>>);

/// 液化的方式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// 顺着笔移动的方向把像素推着走
    Push,
    /// 绕笔刷中心顺时针转
    TwirlCw,
    /// 逆时针转
    TwirlCcw,
    /// 往笔刷中心收
    Pinch,
    /// 从笔刷中心往外鼓
    Bloat,
    /// 往液化之前的样子拉回
    Restore,
}

impl Op {
    pub const ALL: [Op; 6] = [Op::Push, Op::TwirlCw, Op::TwirlCcw, Op::Pinch, Op::Bloat, Op::Restore];

    /// 设置文件里的名字。
    pub fn id(self) -> &'static str {
        match self {
            Op::Push => "push",
            Op::TwirlCw => "twirl_cw",
            Op::TwirlCcw => "twirl_ccw",
            Op::Pinch => "pinch",
            Op::Bloat => "bloat",
            Op::Restore => "restore",
        }
    }

    pub fn from_id(id: &str) -> Option<Op> {
        Op::ALL.into_iter().find(|op| op.id() == id)
    }
}

/// 液化笔刷。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Brush {
    pub op: Op,
    /// 直径，画布像素
    pub size: f32,
    /// 0 到 1
    pub strength: f32,
}

/// 转、收、鼓、复原随时间起作用。下面是强度 100%、笔刷中心处每秒的量。
/// 转的角度（弧度）：每秒半圈。
const TWIRL_PER_SECOND: f32 = PI;
/// 收、鼓：每秒缩放一倍（这里是它的对数）。
const SCALE_PER_SECOND: f32 = LN_2;
/// 复原：每秒拉回约 95%。
const RESTORE_PER_SECOND: f32 = 3.0;
/// 一下最多按这么长的时间算：中间隔得太久（比如程序卡了一下），不要一下子跳出去很远。
const MAX_DT: f32 = 0.05;
/// 复原时短于这个的偏移直接归零，这样复原到底就是真的回到原图。
const NEGLIGIBLE: f32 = 1.0 / 256.0;
/// 一下碰到的像素不少于这么多时，按分块分给几个线程去算。
const PARALLEL_PIXELS: i64 = 60_000;

/// 位移场的一个分块：每个像素一个偏移 (dx, dy)，外加这块里偏移的最大长度。
#[derive(Clone)]
struct Cell {
    d: Vec<[f32; 2]>,
    max: f32,
}

/// 位移场。按 64×64 分块稀疏存储，没有分块的地方偏移为零；分块写时复制，存快照不费事。
#[derive(Clone, Default)]
struct Field {
    tiles: HashMap<TileCoord, Arc<Cell>>,
}

impl Field {
    /// 一次拿到多个分块（没有的先建成全零），可以分给几个线程各写各的。
    /// 给出的是分块的引用本身：被快照共用着的分块要先复制一份才能写，这一步也留给各个线程自己做。
    fn many_mut(&mut self, coords: &[TileCoord]) -> Vec<(TileCoord, &mut Arc<Cell>)> {
        for &c in coords {
            self.tiles.entry(c).or_insert_with(|| Arc::new(Cell { d: vec![[0.0; 2]; N], max: 0.0 }));
        }
        let wanted: HashSet<TileCoord> = coords.iter().copied().collect();
        self.tiles.iter_mut().filter(|(c, _)| wanted.contains(*c)).map(|(c, t)| (*c, t)).collect()
    }

    /// 和另一个位移场不一样的分块。
    fn differs(&self, other: &Field) -> Vec<TileCoord> {
        let mut out: Vec<TileCoord> = self.tiles.iter().filter(|(tc, cell)| !other.tiles.get(tc).is_some_and(|o| Arc::ptr_eq(o, cell))).map(|(tc, _)| *tc).collect();
        out.extend(other.tiles.keys().filter(|tc| !self.tiles.contains_key(tc)));
        out
    }
}

/// 读位移场用：一块范围里的分块排成一张表，取值时由坐标直接算出下标，不查哈希。
struct View<'a> {
    tx0: i32,
    ty0: i32,
    w: i32,
    h: i32,
    cells: Vec<Option<&'a [[f32; 2]]>>,
}

impl<'a> View<'a> {
    fn new(field: &'a HashMap<TileCoord, Arc<Cell>>, r: Rect) -> Self {
        let (tx0, ty0) = (r.x0.div_euclid(TILE), r.y0.div_euclid(TILE));
        let (w, h) = ((r.x1 - 1).div_euclid(TILE) - tx0 + 1, (r.y1 - 1).div_euclid(TILE) - ty0 + 1);
        let cells = (0..h).flat_map(|j| (0..w).map(move |i| (tx0 + i, ty0 + j))).map(|tc| field.get(&tc).map(|c| c.d.as_slice())).collect();
        View { tx0, ty0, w, h, cells }
    }

    /// 像素 (x, y) 所在的分块。表外和没有分块的地方是 None。
    #[inline]
    fn cell(&self, x: i32, y: i32) -> Option<&'a [[f32; 2]]> {
        let (i, j) = ((x >> SHIFT) - self.tx0, (y >> SHIFT) - self.ty0);
        if i < 0 || j < 0 || i >= self.w || j >= self.h {
            return None;
        }
        self.cells[(j * self.w + i) as usize]
    }

    /// 像素 (x, y) 的偏移。表外和没有分块的地方是零。
    #[inline]
    fn get(&self, x: i32, y: i32) -> [f32; 2] {
        self.cell(x, y).map_or([0.0; 2], |d| d[(((y & MASK) << SHIFT) | (x & MASK)) as usize])
    }

    /// 连续坐标 (x, y) 处双线性插值出来的偏移（像素中心在 +0.5 处）。
    #[inline]
    fn sample(&self, x: f32, y: f32) -> [f32; 2] {
        let (fx, fy) = (x - 0.5, y - 0.5);
        // 向下取整：只截掉小数再修正负数。`floor` 在没有指定处理器型号时是一次函数调用，逐像素用太慢。
        let down = |v: f32| {
            let i = v as i32;
            if (i as f32) > v {
                i - 1
            } else {
                i
            }
        };
        let (ix, iy) = (down(fx), down(fy));
        let (tx, ty) = (fx - ix as f32, fy - iy as f32);
        let (a, b, c, d) = if (ix & MASK) < MASK && (iy & MASK) < MASK {
            // 四个像素在同一个分块里：只找一次分块。
            let Some(t) = self.cell(ix, iy) else { return [0.0; 2] };
            let o = (((iy & MASK) << SHIFT) | (ix & MASK)) as usize;
            (t[o], t[o + 1], t[o + TILE as usize], t[o + TILE as usize + 1])
        } else {
            (self.get(ix, iy), self.get(ix + 1, iy), self.get(ix, iy + 1), self.get(ix + 1, iy + 1))
        };
        let mix = |k: usize| (a[k] * (1.0 - tx) + b[k] * tx) * (1.0 - ty) + (c[k] * (1.0 - tx) + d[k] * tx) * ty;
        [mix(0), mix(1)]
    }
}

/// 位移场当作一般的映射：目标像素 p 从源图的 p + D(p) 取色。
struct FieldMap<'a>(&'a Field);

impl Mapping for FieldMap<'_> {
    fn reach(&self) -> Rect {
        self.0.tiles.keys().fold(Rect::EMPTY, |r, &tc| r.union(tile_rect(tc)))
    }

    fn sources(&self, r: Rect, out: &mut [(f64, f64)]) {
        let w = (r.x1 - r.x0) as usize + 1;
        for (j, row) in out.chunks_mut(w).enumerate() {
            let y = r.y0 + j as i32;
            let mut cur: Option<(i32, Option<&Arc<Cell>>)> = None;
            for (i, o) in row.iter_mut().enumerate() {
                let x = r.x0 + i as i32;
                let tx = x.div_euclid(TILE);
                let cell = match cur {
                    Some((t, c)) if t == tx => c,
                    _ => {
                        let c = self.0.tiles.get(&(tx, y.div_euclid(TILE)));
                        cur = Some((tx, c));
                        c
                    }
                };
                let d = cell.map_or([0.0; 2], |c| c.d[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize]);
                // 偏移为零时正好是像素自己的中心，重采样那边会原样照搬。
                *o = (x as f64 + 0.5 + d[0] as f64, y as f64 + 0.5 + d[1] as f64);
            }
        }
    }
}

/// 一次液化：从开始到确定或取消。
pub struct Liquify {
    pub layer: usize,
    /// 开始液化时这一层的像素
    orig: TileMap,
    /// 开始时的选区：没选中的像素不会动
    sel: Option<Selection>,
    /// 保护不透明度：透明度保持原样，只有颜色跟着动
    keep_alpha: bool,
    clip: Rect,
    field: Field,
    /// 每一笔开始前的位移场，撤销、重做在这些快照之间来回
    undo: Vec<Field>,
    redo: Vec<Field>,
    /// 这一笔上一下落在哪里；不在笔画中时为 None
    last: Option<(f32, f32)>,
    /// 这一笔改过位移场
    changed: bool,
    /// 位移场变了、预览像素还没跟上的分块
    pending: HashSet<TileCoord>,
}

impl Liquify {
    /// 对当前图层开始液化。当前图层锁定或者隐藏着时不能液化。
    pub fn begin(doc: &Document) -> Option<Liquify> {
        if !doc.can_edit_active() {
            return None;
        }
        let layer = doc.active();
        let l = &doc.layers()[layer];
        Some(Liquify {
            layer,
            orig: l.tiles.clone(),
            sel: doc.selection().cloned(),
            keep_alpha: l.props.lock_alpha,
            clip: doc.bounds(),
            field: Field::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            last: None,
            changed: false,
            pending: HashSet::new(),
        })
    }

    /// 开始液化时的像素。
    pub fn orig(&self) -> &TileMap {
        &self.orig
    }

    /// 位移场不全是零：确定时会改动图层。
    pub fn touched(&self) -> bool {
        !self.field.tiles.is_empty()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 落笔。
    pub fn press(&mut self, at: (f32, f32)) {
        self.undo.push(self.field.clone());
        self.redo.clear();
        self.last = Some(at);
        self.changed = false;
    }

    /// 抬笔。这一笔什么都没改的话，不留撤销记录。
    pub fn release(&mut self) {
        if self.last.take().is_some() && !self.changed {
            self.undo.pop();
        }
    }

    /// 笔在 `at`，压力 `pressure`（0 到 1），离上一下过了 `dt` 秒。返回位移场变了的范围。
    pub fn dab(&mut self, brush: &Brush, at: (f32, f32), pressure: f32, dt: f32) -> Rect {
        let Some(last) = self.last.replace(at) else { return Rect::EMPTY };
        let r = (brush.size / 2.0).max(1.0);
        let k = brush.strength.clamp(0.0, 1.0) * pressure.clamp(0.0, 1.0);
        let dt = dt.clamp(0.0, MAX_DT);
        if k <= 0.0 {
            return Rect::EMPTY;
        }
        // 新画面在 p 处的颜色 = 旧画面在 q(p) 处的颜色，于是新的偏移 = q + 旧偏移(q) − p。
        let moved = |q: (f32, f32), p: (f32, f32), v: &View| {
            let s = v.sample(q.0, q.1);
            [q.0 + s[0] - p.0, q.1 + s[1] - p.1]
        };
        match brush.op {
            Op::Push => {
                let d = (at.0 - last.0, at.1 - last.1);
                let len = d.0.hypot(d.1);
                if len < 1e-3 {
                    return Rect::EMPTY;
                }
                // 一次挪得太远就拆成几步，每步不超过四分之一个半径，推出来的形状才连贯。
                let steps = (len / (r / 4.0)).ceil().clamp(1.0, 256.0) as usize;
                let step = (d.0 / steps as f32, d.1 / steps as f32);
                let mut dirty = Rect::EMPTY;
                for s in 1..=steps {
                    let c = (last.0 + step.0 * s as f32, last.1 + step.1 * s as f32);
                    dirty = dirty.union(self.apply(c, r, k, |p, w, v, _, _| moved((p.0 - w * step.0, p.1 - w * step.1), p, v)));
                }
                dirty
            }
            Op::TwirlCw | Op::TwirlCcw => {
                // 画面顺时针转 θ，就是每一点从逆时针转 θ 的地方取色（y 轴向下，正角度在屏幕上是顺时针）。
                let sign = if brush.op == Op::TwirlCw { -1.0 } else { 1.0 };
                let turn = sign * TWIRL_PER_SECOND * dt;
                if turn == 0.0 {
                    return Rect::EMPTY;
                }
                self.apply(at, r, k, |p, w, v, _, _| {
                    let (s, c) = (turn * w).sin_cos();
                    let (x, y) = (p.0 - at.0, p.1 - at.1);
                    moved((at.0 + c * x - s * y, at.1 + s * x + c * y), p, v)
                })
            }
            Op::Pinch | Op::Bloat => {
                // 收：从离中心更远的地方取色，画面往里缩；鼓反过来。
                let sign = if brush.op == Op::Pinch { 1.0 } else { -1.0 };
                let rate = sign * SCALE_PER_SECOND * dt;
                if rate == 0.0 {
                    return Rect::EMPTY;
                }
                self.apply(at, r, k, |p, w, v, _, _| {
                    let f = (rate * w).exp();
                    moved((at.0 + (p.0 - at.0) * f, at.1 + (p.1 - at.1) * f), p, v)
                })
            }
            Op::Restore => {
                let rate = RESTORE_PER_SECOND * dt;
                if rate == 0.0 {
                    return Rect::EMPTY;
                }
                self.apply(at, r, k, |_, w, v, x, y| {
                    let (d, f) = (v.get(x, y), (-rate * w).exp());
                    let o = [d[0] * f, d[1] * f];
                    if o[0].hypot(o[1]) < NEGLIGIBLE {
                        [0.0; 2]
                    } else {
                        o
                    }
                })
            }
        }
    }

    /// 以 `c` 为中心、半径 `r` 的一下：圆里每个像素的新偏移由 `f` 给出。
    /// `f` 拿到像素中心 p、这里的权重 w、这一下之前的位移场，以及像素的下标。权重为零的像素不动。
    fn apply(&mut self, c: (f32, f32), r: f32, k: f32, f: impl Fn((f32, f32), f32, &View, i32, i32) -> [f32; 2] + Sync) -> Rect {
        let bounds = Rect { x0: (c.0 - r).floor() as i32, y0: (c.1 - r).floor() as i32, x1: (c.0 + r).ceil() as i32 + 1, y1: (c.1 + r).ceil() as i32 + 1 }.intersect(self.clip);
        if bounds.is_empty() {
            return Rect::EMPTY;
        }
        // 读的是这一下之前的位移场（分块共享，取快照不复制）。取色点离像素不会超过一个半径，表多铺一圈。
        let before = self.field.tiles.clone();
        let g = r.ceil() as i32 + 2;
        let view = View::new(&before, Rect { x0: bounds.x0 - g, y0: bounds.y0 - g, x1: bounds.x1 + g, y1: bounds.y1 + g });
        let r2 = r * r;
        // 外接方框四个角上那几块碰不到圆，不用去动。
        let reaches = |tc: TileCoord| {
            let t = tile_rect(tc);
            let (dx, dy) = ((t.x0 as f32 - c.0).max(c.0 - t.x1 as f32).max(0.0), (t.y0 as f32 - c.1).max(c.1 - t.y1 as f32).max(0.0));
            dx * dx + dy * dy < r2
        };
        let coords: Vec<TileCoord> = tiles_in(bounds).filter(|&tc| reaches(tc)).collect();
        let sel = self.sel.as_ref();
        // 返回这一块里有没有像素被改过。
        let paint = |tc: TileCoord, slot: &mut Arc<Cell>| -> bool {
            let cover = match sel.map(|s| s.tile(tc)) {
                Some(None) => return false,
                Some(Some(s)) => Some(s),
                None => None,
            };
            let cell = Arc::make_mut(slot);
            let area = tile_rect(tc).intersect(bounds);
            let mut wrote = false;
            for py in area.y0..area.y1 {
                let cy = py as f32 + 0.5;
                let dy = cy - c.1;
                if dy * dy >= r2 {
                    continue;
                }
                // 只走这一行里落在圆内的那一段。
                let half = (r2 - dy * dy).sqrt();
                let xa = ((c.0 - half - 0.5).floor() as i32).max(area.x0);
                let xb = ((c.0 + half - 0.5).ceil() as i32 + 1).min(area.x1);
                for px in xa..xb {
                    let cx = px as f32 + 0.5;
                    let t = ((cx - c.0) * (cx - c.0) + dy * dy) / r2;
                    if t >= 1.0 {
                        continue;
                    }
                    let i = ((py - tc.1 * TILE) * TILE + (px - tc.0 * TILE)) as usize;
                    // 圆心最强，到边上平滑地降到零（斜率也是零），没有硬边。
                    let mut w = k * (1.0 - t) * (1.0 - t);
                    if let Some(s) = cover {
                        w *= s[i] as f32 / ONE as f32;
                    }
                    if w > 0.0 {
                        cell.d[i] = f((cx, cy), w, &view, px, py);
                        wrote = true;
                    }
                }
            }
            if wrote {
                cell.max = cell.d.iter().map(|d| d[0] * d[0] + d[1] * d[1]).fold(0.0, f32::max).sqrt();
            }
            wrote
        };
        let area = (bounds.x1 - bounds.x0) as i64 * (bounds.y1 - bounds.y0) as i64;
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
        let mut cells = self.field.many_mut(&coords);
        let wrote: Vec<TileCoord> = if area >= PARALLEL_PIXELS && threads > 1 && cells.len() > 1 {
            let per = cells.len().div_ceil(threads);
            let paint = &paint;
            std::thread::scope(|scope| {
                let jobs: Vec<_> = cells.chunks_mut(per).map(|part| scope.spawn(move || part.iter_mut().filter_map(|(tc, cell)| paint(*tc, cell).then_some(*tc)).collect::<Vec<_>>())).collect();
                jobs.into_iter().flat_map(|j| j.join().unwrap()).collect()
            })
        } else {
            cells.iter_mut().filter_map(|(tc, cell)| paint(*tc, cell).then_some(*tc)).collect()
        };
        drop(cells);
        // 只有真的改过的分块要重算预览：拖动时笔刷外接方框的四个角、笔刷后面已经推过的地方都不用。
        self.pending.extend(wrote);
        // 全是零的分块不留着：没碰过的地方和复原到底的地方都一样不占内存，确定时也不用重算。
        for tc in &coords {
            if self.field.tiles.get(tc).is_some_and(|c| c.max == 0.0) {
                self.field.tiles.remove(tc);
            }
        }
        self.changed = true;
        bounds
    }

    /// 撤销这次液化里的上一笔。没有可撤销的时返回 false。画面要等 `take_preview` 再重算。
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        let now = std::mem::replace(&mut self.field, prev);
        self.pending.extend(now.differs(&self.field));
        self.redo.push(now);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        let now = std::mem::replace(&mut self.field, next);
        self.pending.extend(now.differs(&self.field));
        self.undo.push(now);
        true
    }

    /// 位移场变了的分块按现在的位移场重算（快速档）。返回它们的范围和要换进图层显示的分块。
    pub fn take_preview(&mut self) -> (Rect, Vec<TileSlot>) {
        let coords: Vec<TileCoord> = self.pending.drain().filter(|&tc| !tile_rect(tc).intersect(self.clip).is_empty()).collect();
        let area = coords.iter().fold(Rect::EMPTY, |r, &tc| r.union(tile_rect(tc))).intersect(self.clip);
        (area, self.render(&coords, Quality::Fast))
    }

    /// 确定用：位移场碰过的分块按高质量档重算，其余分块照旧共用原图。返回整层的像素。
    pub fn result(&self) -> TileMap {
        let coords: Vec<TileCoord> = self.field.tiles.keys().copied().collect();
        let mut out = self.orig.clone();
        for (tc, t) in self.render(&coords, Quality::Best) {
            out.replace(tc, t);
        }
        out
    }

    fn render(&self, coords: &[TileCoord], quality: Quality) -> Vec<TileSlot> {
        // 位移场没碰过的分块就是原图，直接共用。
        let (moved, still): (Vec<TileCoord>, Vec<TileCoord>) = coords.iter().partition(|tc| self.field.tiles.contains_key(tc));
        let mut out: Vec<TileSlot> = still.into_iter().map(|tc| (tc, self.orig.get(tc).cloned())).collect();
        if moved.is_empty() {
            return out;
        }
        // 可能取到的源范围：这些分块向外扩出其中最大的偏移。
        let (mut area, mut reach) = (Rect::EMPTY, 0f32);
        for tc in &moved {
            area = area.union(tile_rect(*tc));
            reach = reach.max(self.field.tiles[tc].max);
        }
        let g = reach.ceil() as i32 + 3;
        let from = Rect { x0: area.x0.saturating_sub(g), y0: area.y0.saturating_sub(g), x1: area.x1.saturating_add(g), y1: area.y1.saturating_add(g) };
        for (tc, t) in transform::map_some(&self.orig, &FieldMap(&self.field), quality, self.clip, &moved, from) {
            let t = if self.keep_alpha { keep_alpha(self.orig.get(tc), t) } else { t };
            // 只改画布里的；分块伸出画布的那部分照旧。
            out.push((tc, transform::keep_outside(t, self.orig.get(tc), tc, self.clip)));
        }
        out
    }
}

/// 保护不透明度：透明度换回原来的，颜色按新的透明度折算过去；新的这里是空的就留着原来的像素。
fn keep_alpha(orig: Option<&Arc<Tile>>, new: Option<Arc<Tile>>) -> Option<Arc<Tile>> {
    let o = orig?;
    let mut t = Tile::new();
    let empty = Tile::new();
    let n = new.as_deref().unwrap_or(&empty);
    let mut any = false;
    for ((d, p), q) in t.0.as_chunks_mut::<4>().0.iter_mut().zip(n.0.as_chunks::<4>().0).zip(o.0.as_chunks::<4>().0) {
        let a = q[3] as u32;
        *d = if a == 0 {
            [0; 4]
        } else if p[3] == 0 {
            *q
        } else {
            let scale = |c: u16| ((c as u32 * a + p[3] as u32 / 2) / p[3] as u32).min(a) as u16;
            [scale(p[0]), scale(p[1]), scale(p[2]), a as u16]
        };
        any |= a != 0;
    }
    any.then(|| Arc::new(t))
}

fn tile_rect(tc: TileCoord) -> Rect {
    Rect { x0: tc.0 * TILE, y0: tc.1 * TILE, x1: (tc.0 + 1) * TILE, y1: (tc.1 + 1) * TILE }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::LayerProps;
    use crate::view::{Display, View as Screen};
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

    const INK: [u16; 4] = [0, 0, 0, ONE as u16];

    /// w×h 的文档，当前图层上画着 `shapes` 里的东西。
    fn doc_with(w: i32, h: i32, shapes: &[(Rect, [u16; 4])]) -> Document {
        let mut doc = Document::new(w, h);
        let mut tiles = TileMap::default();
        for &(r, c) in shapes {
            paint(&mut tiles, r, |_, _| c);
        }
        doc.set_layer_tiles_live(0, tiles);
        doc
    }

    fn brush(op: Op, size: f32) -> Brush {
        Brush { op, size, strength: 1.0 }
    }

    /// 画一笔：从 `from` 走到 `to`，分 `steps` 下，每下隔 `dt` 秒。
    fn stroke(l: &mut Liquify, b: &Brush, from: (f32, f32), to: (f32, f32), steps: usize, dt: f32) -> Rect {
        l.press(from);
        let mut dirty = Rect::EMPTY;
        for s in 1..=steps {
            let t = s as f32 / steps as f32;
            dirty = dirty.union(l.dab(b, (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t), 1.0, dt));
        }
        l.release();
        dirty
    }

    fn alpha_sum(tiles: &TileMap, area: Rect) -> f64 {
        (area.y0..area.y1).flat_map(|y| (area.x0..area.x1).map(move |x| (x, y))).map(|(x, y)| at(tiles, x, y)[3] as f64 / ONE as f64).sum()
    }

    /// (x, y) 周围 5×5 里的墨量（以像素计）。
    fn around(tiles: &TileMap, x: i32, y: i32) -> f64 {
        alpha_sum(tiles, rect(x - 2, y - 2, x + 3, y + 3))
    }

    #[test]
    fn pushing_moves_pixels_along_with_the_pen() {
        // 一条两像素宽的竖线，从左往右推过它的中段。
        let doc = doc_with(300, 200, &[(rect(99, 0, 101, 200), INK)]);
        let orig = &doc.layers()[0].tiles;
        let mut l = Liquify::begin(&doc).unwrap();
        let dirty = stroke(&mut l, &brush(Op::Push, 80.0), (60.0, 100.0), (140.0, 100.0), 16, 0.0);
        assert!(dirty.x0 <= 25 && dirty.x1 >= 175 && dirty.y0 <= 65 && dirty.y1 >= 135, "{dirty:?}");
        let out = l.result();
        // 笔刷从线的左边推过来：线在笔刷前面被一路往右推（越靠近中心走得越快，所以挤在笔刷前沿，变细了），
        // 停在笔刷中心的右边；原来的位置空了。
        assert!(around(&out, 100, 100) < 2.0, "{}", around(&out, 100, 100));
        let row = |y: i32| (80..200).max_by_key(|&x| at(&out, x, y)[3]).unwrap();
        assert!(row(100) > 140 && row(100) < 160, "{}", row(100));
        assert!(alpha_sum(&out, rect(80, 100, 200, 101)) > 0.15);
        // 离笔刷超过一个半径的地方一位不差。
        for y in (0..58).chain(143..200) {
            for x in 90..110 {
                assert_eq!(at(&out, x, y), at(orig, x, y), "({x}, {y})");
            }
        }
        // 线没有被推断：中段往右弯过去，每一行都还有墨，越往两头弯得越少。
        for y in 0..200 {
            assert!(alpha_sum(&out, rect(80, y, 200, y + 1)) > 0.15, "第 {y} 行");
        }
        assert!(row(80) > 101 && row(80) < row(100), "{} {}", row(80), row(100));
    }

    #[test]
    fn restoring_all_the_way_gives_back_the_original_exactly() {
        let doc = doc_with(256, 256, &[(rect(60, 40, 200, 220), [ONE as u16 / 2, 0, ONE as u16 / 4, ONE as u16]), (rect(120, 0, 128, 256), INK)]);
        let mut l = Liquify::begin(&doc).unwrap();
        stroke(&mut l, &brush(Op::Push, 100.0), (60.0, 128.0), (200.0, 140.0), 20, 0.0);
        stroke(&mut l, &brush(Op::TwirlCw, 120.0), (128.0, 128.0), (130.0, 128.0), 10, 0.05);
        assert!(l.touched());
        // 用一支大笔把整张都复原回去。
        let big = brush(Op::Restore, 600.0);
        l.press((128.0, 128.0));
        for _ in 0..200 {
            l.dab(&big, (128.0, 128.0), 1.0, 0.05);
        }
        l.release();
        assert!(!l.touched(), "位移场应当全部归零");
        let out = l.result();
        for tc in l.orig().coords() {
            assert!(Arc::ptr_eq(out.get(tc).unwrap(), l.orig().get(tc).unwrap()), "{tc:?}");
        }
        assert_eq!(out.len(), l.orig().len());
    }

    #[test]
    fn twirling_turns_the_right_way() {
        // 一条横线穿过笔刷中心。顺时针转时（y 轴向下），右半边往下走、左半边往上走。
        let doc = doc_with(300, 200, &[(rect(0, 99, 300, 101), INK)]);
        for (op, down) in [(Op::TwirlCw, true), (Op::TwirlCcw, false)] {
            let mut l = Liquify::begin(&doc).unwrap();
            stroke(&mut l, &brush(op, 120.0), (150.0, 100.0), (150.0, 100.0), 10, 0.05);
            let out = l.result();
            // 离中心 20 像素处权重约 0.79，半秒转 90° 的 0.79 倍，约 71°。
            let (below, above) = (around(&out, 156, 119), around(&out, 156, 81));
            if down {
                assert!(below > 2.0 && above < 0.5, "{op:?}: {below} {above}");
            } else {
                assert!(above > 2.0 && below < 0.5, "{op:?}: {below} {above}");
            }
            // 原来的位置上已经没有线了，笔刷外面的线没动。
            assert!(around(&out, 170, 100) < 0.5);
            assert_eq!(at(&out, 20, 100), INK);
        }
    }

    #[test]
    fn pinching_shrinks_and_bloating_grows() {
        let mut doc = Document::new(300, 200);
        let mut tiles = TileMap::default();
        paint(&mut tiles, rect(110, 60, 190, 140), |x, y| {
            let d = ((x as f32 + 0.5 - 150.0).powi(2) + (y as f32 + 0.5 - 100.0).powi(2)).sqrt();
            if d < 30.0 {
                INK
            } else {
                [0; 4]
            }
        });
        doc.set_layer_tiles_live(0, tiles);
        let before = alpha_sum(&doc.layers()[0].tiles, doc.bounds());
        let mut sizes = Vec::new();
        for op in [Op::Pinch, Op::Bloat] {
            let mut l = Liquify::begin(&doc).unwrap();
            stroke(&mut l, &brush(op, 160.0), (150.0, 100.0), (150.0, 100.0), 10, 0.05);
            sizes.push(alpha_sum(&l.result(), doc.bounds()));
        }
        assert!(sizes[0] < before * 0.8 && sizes[1] > before * 1.2, "{before} {sizes:?}");
    }

    #[test]
    fn unselected_pixels_never_move() {
        let mut doc = Document::new(256, 200);
        let mut tiles = TileMap::default();
        paint(&mut tiles, doc.bounds(), |x, y| [(x * 97 % 251) as u16 * 100, (y * 31 % 241) as u16 * 100, 0, ONE as u16]);
        doc.set_layer_tiles_live(0, tiles);
        doc.set_selection(Some(Selection::rect(rect(0, 0, 128, 200))));
        let mut l = Liquify::begin(&doc).unwrap();
        stroke(&mut l, &brush(Op::Push, 120.0), (60.0, 100.0), (200.0, 100.0), 20, 0.0);
        stroke(&mut l, &brush(Op::Bloat, 120.0), (128.0, 100.0), (128.0, 100.0), 10, 0.05);
        let out = l.result();
        let orig = &doc.layers()[0].tiles;
        for y in 0..200 {
            for x in 128..256 {
                assert_eq!(at(&out, x, y), at(orig, x, y), "({x}, {y})");
            }
        }
        assert!((0..128).any(|x| at(&out, x, 100) != at(orig, x, 100)), "选区里应当动了");
    }

    #[test]
    fn protected_opacity_keeps_alpha_exactly() {
        let mut doc = Document::new(256, 200);
        let mut tiles = TileMap::default();
        paint(&mut tiles, rect(40, 40, 220, 160), |x, y| {
            let a = ((x * 13 + y * 7) % 200) as u32 * ONE / 200;
            [(a / 3) as u16, (a / 2) as u16, (if x < 128 { a } else { 0 }) as u16, a as u16]
        });
        doc.set_layer_tiles_live(0, tiles);
        let props = doc.layers()[0].props.clone();
        doc.set_props_live(0, LayerProps { lock_alpha: true, ..props });
        let mut l = Liquify::begin(&doc).unwrap();
        stroke(&mut l, &brush(Op::Push, 100.0), (60.0, 100.0), (200.0, 110.0), 20, 0.0);
        let out = l.result();
        let orig = &doc.layers()[0].tiles;
        let mut changed = false;
        for y in 0..200 {
            for x in 0..256 {
                let (p, q) = (at(&out, x, y), at(orig, x, y));
                assert_eq!(p[3], q[3], "({x}, {y})");
                assert!(p[..3].iter().all(|&c| c <= p[3]), "({x}, {y}) {p:?}");
                changed |= p != q;
            }
        }
        assert!(changed, "颜色应当跟着动");
    }

    #[test]
    fn strokes_can_be_undone_and_redone_within_the_session() {
        let doc = doc_with(256, 256, &[(rect(100, 0, 110, 256), INK), (rect(0, 120, 256, 130), INK)]);
        let mut l = Liquify::begin(&doc).unwrap();
        assert!(!l.can_undo() && !l.undo());
        // 压力为零的一笔什么都不改，不留撤销记录。
        l.press((50.0, 50.0));
        l.dab(&brush(Op::Push, 60.0), (80.0, 60.0), 0.0, 0.0);
        l.release();
        assert!(!l.can_undo());
        stroke(&mut l, &brush(Op::Push, 80.0), (60.0, 60.0), (160.0, 60.0), 10, 0.0);
        let first = l.result();
        stroke(&mut l, &brush(Op::Push, 80.0), (60.0, 200.0), (160.0, 190.0), 10, 0.0);
        let second = l.result();
        let same = |a: &TileMap, b: &TileMap| (0..256).all(|y| (0..256).all(|x| at(a, x, y) == at(b, x, y)));
        assert!(!same(&first, &second));
        l.take_preview();
        assert!(l.undo());
        let (dirty, _) = l.take_preview();
        assert!(dirty.y1 > 150 && dirty.y0 >= 64, "只有第二笔碰过的地方要重算：{dirty:?}");
        assert!(same(&l.result(), &first));
        assert!(l.undo());
        assert!(!l.touched() && !l.undo());
        assert!(l.redo() && l.redo());
        assert!(same(&l.result(), &second) && !l.redo());
        // 撤销之后再画一笔，重做的记录就没了。
        assert!(l.undo());
        stroke(&mut l, &brush(Op::Bloat, 80.0), (100.0, 125.0), (100.0, 125.0), 4, 0.05);
        assert!(!l.can_redo());
    }

    #[test]
    fn preview_is_close_to_the_result_and_untouched_tiles_stay_shared() {
        let doc = doc_with(640, 480, &[(rect(0, 0, 640, 480), [ONE as u16 / 3, ONE as u16 / 2, ONE as u16 / 5, ONE as u16]), (rect(300, 0, 304, 480), INK)]);
        let mut l = Liquify::begin(&doc).unwrap();
        stroke(&mut l, &brush(Op::Push, 100.0), (250.0, 240.0), (360.0, 250.0), 12, 0.0);
        let out = l.result();
        let orig = &doc.layers()[0].tiles;
        let mut shared = 0;
        for tc in orig.coords() {
            if !l.field.tiles.contains_key(&tc) {
                assert!(Arc::ptr_eq(out.get(tc).unwrap(), orig.get(tc).unwrap()), "{tc:?}");
                shared += 1;
            }
        }
        assert!(shared > 40, "{shared}");
        let mut live = orig.clone();
        let (area, part) = l.take_preview();
        assert!(!area.is_empty() && l.take_preview().1.is_empty(), "取过一次之后没有要重算的了");
        for (tc, t) in part {
            live.replace(tc, t);
        }
        // 快速档和高质量档只差在插值上。
        let diff: f64 = (0..480).flat_map(|y| (0..640).map(move |x| (x, y))).map(|(x, y)| (at(&live, x, y)[3] as f64 - at(&out, x, y)[3] as f64).abs() / ONE as f64).sum();
        assert!(diff < 40.0, "{diff}");
    }

    #[test]
    fn locked_or_hidden_layers_cannot_be_liquified() {
        let mut doc = Document::new(64, 64);
        let props = doc.layers()[0].props.clone();
        doc.set_props_live(0, LayerProps { locked: true, ..props.clone() });
        assert!(Liquify::begin(&doc).is_none());
        doc.set_props_live(0, LayerProps { visible: false, ..props });
        assert!(Liquify::begin(&doc).is_none());
    }

    #[test]
    fn ops_have_stable_names() {
        for op in Op::ALL {
            assert_eq!(Op::from_id(op.id()), Some(op));
        }
        assert_eq!(Op::from_id("smudge"), None);
    }

    /// 大笔刷液化时每来一个笔样本要多久：位移场、预览重算、显示各占多少。平时不跑：
    /// `cargo test --release -p zhibi-core liquify_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn liquify_timing() {
        let cases = [(Op::Push, 500.0, 5.0, 1.0), (Op::Push, 500.0, 20.0, 1.0), (Op::TwirlCw, 500.0, 5.0, 1.0), (Op::Restore, 500.0, 5.0, 1.0), (Op::Push, 500.0, 5.0, 0.56), (Op::Push, 100.0, 5.0, 1.0), (Op::Push, 10.0, 5.0, 1.0)];
        for (op, size, step, zoom) in cases {
            let mut doc = Document::new(3000, 2000);
            let mut tiles = TileMap::default();
            paint(&mut tiles, doc.bounds(), |x, y| {
                let a = (ONE / 4 + ((x * 7 + y * 13) as u32 % (ONE * 3 / 4))) as u16;
                [a / 2, (x as u32 * 16 % a as u32) as u16, a, a]
            });
            doc.set_layer_tiles_live(0, tiles);
            let view = Screen { zoom, angle: 0.0, flip: false, center: (1500.0, 1000.0) };
            let vp = (1816, 1407);
            let mut buf = vec![0u32; (vp.0 * vp.1) as usize];
            let mut display = Display::default();
            display.render(&doc, None, &view, vp, &mut buf, vp.0 as usize, (0, 0), Rect { x0: 0, y0: 0, x1: vp.0, y1: vp.1 });
            let mut l = Liquify::begin(&doc).unwrap();
            // 复原要先有东西可复原。
            if op == Op::Restore {
                stroke(&mut l, &brush(Op::Push, size), (900.0, 1000.0), (1500.0, 1000.0), 120, 0.0);
            }
            let b = brush(op, size);
            let at = |k: usize| (900.0 + step * k as f32, 1000.0 + 80.0 * (k as f32 * 0.05).sin());
            l.press(at(0));
            let (mut t_dab, mut t_preview, mut t_draw, mut worst) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
            let n = 120;
            for k in 1..=n {
                let t0 = Instant::now();
                l.dab(&b, at(k), 1.0, 0.005);
                let t1 = Instant::now();
                let (dirty, part) = l.take_preview();
                doc.set_layer_part_live(0, part);
                let t2 = Instant::now();
                display.invalidate(dirty);
                display.render(&doc, None, &view, vp, &mut buf, vp.0 as usize, (0, 0), view.screen_bounds(dirty, vp));
                let t3 = Instant::now();
                t_dab += (t1 - t0).as_secs_f64();
                t_preview += (t2 - t1).as_secs_f64();
                t_draw += (t3 - t2).as_secs_f64();
                worst = worst.max((t3 - t0).as_secs_f64());
            }
            l.release();
            let t0 = Instant::now();
            let out = l.result();
            let t_commit = t0.elapsed().as_secs_f64();
            assert!(!out.is_empty());
            println!(
                "{op:?} size {size:>5} step {step:>4} zoom {zoom:<4}: per sample field {:.2} ms, preview {:.2} ms, redraw {:.2} ms, worst {:.2} ms; commit {:.0} ms",
                t_dab * 1e3 / n as f64,
                t_preview * 1e3 / n as f64,
                t_draw * 1e3 / n as f64,
                worst * 1e3,
                t_commit * 1e3
            );
        }
    }
}
