// SPDX-License-Identifier: GPL-3.0-or-later
//! 油漆桶与魔棒：按颜色相近程度选出相连的区域，可闭合线稿上的小缺口。
//!
//! 分四步做，都在平铺的字节数组上进行，内层循环里不查分块表、不做合成：
//!
//! 1. 取样：每个像素与起点颜色的差记成一个字节。按分块用到才算，
//!    所以在大画布上填一小块，不必把整张画布合成一遍。
//! 2. 泛洪：扫描线法找出与起点四连通、色差不超过容差的像素。
//! 3. 缝隙闭合（`gap > 0` 时）：把要穿过窄口才到得了的部分去掉，见 `Field::close_gaps`。
//! 4. 软边：把选区延伸到线条抗锯齿的过渡带底下，见 `Field::soften`。
//!
//! 内存：色差和状态各占每像素一个字节，按整张画布分配（清零分配，没碰过的内存页不占物理内存）；
//! 缝隙闭合另要泛洪范围内每像素一个字节，外加排队用的下标（每个靠近边界的像素 4 字节）。
//! 最大画布 16384×16384 整张填满时，前两项共 512 MB，开缝隙闭合约 770 MB；
//! 结果选区另占每个选中分块 8 KB，即每像素 2 字节。
//!
//! 缝隙闭合是照公开文献里的两个想法自己搭的，没有参照任何一家的代码："困球"分割
//!（Zhang 等，《Vectorizing Cartoon Animations》，IEEE TVCG 2009）和带标记的分水岭
//!（Meyer 的浸没算法）；距离用 Borgefors 1986 的 3-4 倒角距离。这三处出处是凭记忆写的，细节未核实。

use std::sync::Arc;

use crate::document::Document;
use crate::selection::Selection;
use crate::tile::{tiles_in, MaskMap, Tile, TileMap, TILE};
use crate::{Rect, ONE};

const T: usize = TILE as usize;
const N: usize = T * T;

/// 填充取样的来源
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// 只看某一层
    Layer(usize),
    /// 看所有可见图层合成后的画面
    Merged,
}

#[derive(Clone, Copy, Debug)]
pub struct FillOptions {
    /// 颜色容差，0..=255：与起点颜色的差在这个范围内的像素算"同一片"
    pub tolerance: u8,
    /// 缝隙闭合，像素。0 为关闭；n 表示宽度不超过 n 个像素的缺口当作封闭的
    pub gap: u32,
    pub source: Source,
}

// 每个像素的状态。"还没取样"必须是 0：状态数组清零分配，没用到的分块碰都不用碰。
const UNLOADED: u8 = 0;
/// 色差在容差之内，但不在填充区域里（还没填到，或者被缝隙闭合挡在了外面）
const OPEN: u8 = 1;
/// 色差超出容差
const WALL: u8 = 2;
/// 在填充区域里
const IN: u8 = 3;

/// 缝隙闭合的上限（像素）。净空要和两个标记位挤在一个字节里，见 `close_gaps`。
const MAX_GAP: u32 = 40;
// 净空字节的低 6 位是净空，高两位标记这个像素归谁。
const LEVEL: u8 = 0x3f;
const THEIRS: u8 = 0x40;
const MINE: u8 = 0x80;

/// 软边最多向线条里探多少步。硬边笔刷的过渡带只有一两个像素，半软的笔刷有四五个。
/// 探了这么远颜色还在变，就当它是渐变而不是线条的边：不加软边，填充照常停在容差的边界上。
const REACH: usize = 16;

/// 魔棒：从 (x, y) 出发，选出与它相连、颜色相近的区域。起点在画布外返回空选区。
pub fn flood_select(doc: &Document, x: i32, y: i32, opts: &FillOptions) -> Selection {
    let Some(mut field) = Field::new(doc, x, y, opts) else { return Selection::empty() };
    let (sx, sy) = (x as usize, y as usize);
    let Some(mut area) = field.flood(sx, sy) else { return Selection::empty() };
    if opts.gap > 0 {
        area = field.close_gaps(area, sx, sy, opts.gap);
    }
    let soft = field.soften(area);
    field.into_selection(area, &soft)
}

/// 油漆桶：把 flood_select 选出的区域（文档有选区时再与选区求交）用给定颜色、不透明度(0..1)填进当前图层，
/// 记为一个撤销步骤。返回需要重画的画布区域；什么都没填时返回 Rect::EMPTY 且不产生撤销步骤。
///
/// 区域内部是普通的"源覆盖"合成。区域边上那圈只选中了一部分的像素（线条的抗锯齿过渡带）例外：
/// 按选中程度把颜色盖上去的话，盖不住的那部分仍是原来的底色，填充与线条之间会剩一圈最多四分之一深浅的浅边。
/// 所以这圈像素改为"只把其中的底色换成新颜色"，见 `replace_share`。
///
/// 当前图层锁定时不填；保护不透明度时只换颜色，各处原来多透明还是多透明。
pub fn bucket_fill(doc: &mut Document, x: i32, y: i32, color: [u8; 3], opacity: f32, opts: &FillOptions) -> Rect {
    let active = doc.active();
    let opacity = (opacity.clamp(0.0, 1.0) * ONE as f32) as u32;
    if opacity == 0 || doc.layers()[active].props.locked {
        return Rect::EMPTY;
    }
    let region = flood_select(doc, x, y, opts);
    if region.is_empty() {
        return Rect::EMPTY;
    }
    let keep_alpha = doc.layers()[active].props.lock_alpha;
    let src = [to15(color[0]), to15(color[1]), to15(color[2]), ONE];
    // 保护不透明度时，颜色按原有的不透明度 alpha 上：有多不透明就上多少，透明的地方上不了。
    let paint = |alpha: u16| if keep_alpha { src.map(|v| mul15(v, alpha as u32)) } else { src };
    let mut tiles = doc.layers()[active].tiles.clone();
    // 当前图层上"被换掉的颜色"：起点处的像素。取样来源是别的图层或合成画面时，它与取样用的起点色不是一回事。
    let seed = {
        let i = (y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize * 4;
        tiles.get((x.div_euclid(TILE), y.div_euclid(TILE))).map_or([0; 4], |t| [t.0[i], t.0[i + 1], t.0[i + 2], t.0[i + 3]])
    };
    let seed8 = quantize(&seed);
    let selection = doc.selection();
    let area = region.bounds().intersect(doc.bounds());
    // 整块填满同一种不透明颜色的分块共用一份像素（分块写时复制）：给大画布铺底色时既快又不占内存。
    let mut solid: Option<Arc<Tile>> = None;
    let mut dirty = Rect::EMPTY;

    for tc in tiles_in(area) {
        let Some(cover) = region.tile(tc) else { continue };
        let chosen = match selection.map(|s| s.tile(tc)) {
            Some(None) => continue,
            other => other.flatten(),
        };
        let full = |m: &[u16]| m.iter().all(|&v| v as u32 == ONE);
        let changed = if !keep_alpha && opacity == ONE && full(cover) && chosen.is_none_or(full) {
            let solid = solid.get_or_insert_with(|| Arc::new(Tile(src.map(|v| v as u16).repeat(N))));
            let same = tiles.get(tc).is_some_and(|t| t.0 == solid.0);
            if !same {
                tiles.replace(tc, Some(solid.clone()));
            }
            !same
        } else {
            let mut tile = tiles.get(tc).map_or_else(Tile::new, |t| Tile::clone(t));
            let mut changed = false;
            for (i, px) in tile.0.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let weight = mul15(chosen.map_or(ONE, |s| s[i] as u32), opacity);
                let share = cover[i] as u32;
                if share == 0 || weight == 0 || (keep_alpha && px[3] == 0) {
                    continue;
                }
                let mut out = if share == ONE {
                    over(*px, paint(px[3]), weight)
                } else if apart(quantize(px), seed8) <= opts.tolerance {
                    // 过渡带上的像素在当前图层里本来就是底色，说明线条在别的图层上，软边由那一层提供。
                    // 这里要一直填到线芯跟前：按选中程度只填一部分的话，线条底下会透出底色来。
                    // 只有线条占了八分之七以上的像素才逐渐收到 0，免得覆盖率有个台阶。
                    over(*px, paint(px[3]), mul15(weight, (share * 8).min(ONE)))
                } else {
                    replace_share(*px, seed, paint(seed[3]), share, weight)
                };
                if keep_alpha {
                    // 新旧两项各自四舍五入，加起来可能差 1；保护不透明度要一丝不差。
                    out[3] = px[3];
                    out[..3].iter_mut().for_each(|c| *c = (*c).min(px[3]));
                }
                changed |= out != *px;
                *px = out;
            }
            if changed {
                tiles.replace(tc, Some(Arc::new(tile)));
            }
            changed
        };
        if changed {
            dirty = dirty.union(Rect { x0: tc.0 * TILE, y0: tc.1 * TILE, x1: (tc.0 + 1) * TILE, y1: (tc.1 + 1) * TILE }.intersect(area));
        }
    }
    if dirty.is_empty() {
        return Rect::EMPTY;
    }
    doc.replace_layer_tiles(active, tiles);
    dirty
}

/// 画布内一块非空的像素范围，右、下边界不含。
#[derive(Clone, Copy)]
struct Area {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl Area {
    fn pixel(x: usize, y: usize) -> Self {
        Area { x0: x, y0: y, x1: x + 1, y1: y + 1 }
    }

    /// 扩大到包住第 y 行的 [x0, x1) 这一段。
    fn include(&mut self, y: usize, x0: usize, x1: usize) {
        *self = Area { x0: self.x0.min(x0), y0: self.y0.min(y), x1: self.x1.max(x1), y1: self.y1.max(y + 1) };
    }

    fn rect(self) -> Rect {
        Rect { x0: self.x0 as i32, y0: self.y0 as i32, x1: self.x1 as i32, y1: self.y1 as i32 }
    }
}

/// 取样来源，连同起点处的颜色（每通道 8 位）。
enum Sampler<'a> {
    Layer { tiles: &'a TileMap, seed: [u8; 4] },
    Merged { doc: &'a Document, seed: [u8; 3] },
}

/// 一次填充的工作区：整张画布每个像素与起点的色差和状态。
struct Field<'a> {
    sampler: Sampler<'a>,
    w: usize,
    h: usize,
    tolerance: u8,
    /// 与起点颜色的差。只有状态不是 UNLOADED 的像素才有意义
    dist: Vec<u8>,
    state: Vec<u8>,
    /// 合成一个分块用的临时缓冲
    scratch: Vec<u32>,
}

impl<'a> Field<'a> {
    /// 起点在画布外、或者取样的图层不存在时返回 None。
    fn new(doc: &'a Document, x: i32, y: i32, opts: &FillOptions) -> Option<Self> {
        if x < 0 || y < 0 || x >= doc.width || y >= doc.height {
            return None;
        }
        let tc = (x / TILE, y / TILE);
        let i = (y % TILE * TILE + x % TILE) as usize;
        let mut scratch = vec![0; N];
        let sampler = match opts.source {
            Source::Layer(index) => {
                let tiles = &doc.layers().get(index)?.tiles;
                Sampler::Layer { tiles, seed: tiles.get(tc).map_or([0; 4], |t| quantize(&t.0[i * 4..i * 4 + 4])) }
            }
            Source::Merged => {
                doc.composite_tile(None, tc, &mut scratch);
                Sampler::Merged { doc, seed: rgb(scratch[i]) }
            }
        };
        let (w, h) = (doc.width as usize, doc.height as usize);
        Some(Field { sampler, w, h, tolerance: opts.tolerance, dist: vec![0; w * h], state: vec![UNLOADED; w * h], scratch })
    }

    /// 给一个分块（在画布内的部分）取样：算出色差，并按容差分成可填和不可填。
    ///
    /// 每个分块只走一次，不许内联：内联进泛洪的内层循环后，实测整个泛洪慢了三倍。
    #[cold]
    #[inline(never)]
    fn load(&mut self, tx: usize, ty: usize) {
        let Field { sampler, w, h, tolerance, dist, state, scratch } = self;
        let (x0, y0) = (tx * T, ty * T);
        let (x1, y1) = ((x0 + T).min(*w), (y0 + T).min(*h));
        let tc = (tx as i32, ty as i32);
        let tile = match &*sampler {
            Sampler::Layer { tiles, .. } => tiles.get(tc),
            Sampler::Merged { doc, .. } => {
                doc.composite_tile(None, tc, scratch);
                None
            }
        };
        for y in y0..y1 {
            let row = y * *w;
            let dist = &mut dist[row + x0..row + x1];
            let at = (y - y0) * T;
            match (&*sampler, tile) {
                (Sampler::Layer { seed, .. }, Some(t)) => {
                    for (d, px) in dist.iter_mut().zip(t.0[at * 4..].as_chunks::<4>().0) {
                        *d = apart(quantize(px), *seed);
                    }
                }
                // 这一层在这里没有分块：整块都是透明。
                (Sampler::Layer { seed, .. }, None) => dist.fill(apart([0; 4], *seed)),
                (Sampler::Merged { seed, .. }, _) => {
                    for (d, &v) in dist.iter_mut().zip(&scratch[at..]) {
                        let c = rgb(v);
                        *d = c[0].abs_diff(seed[0]).max(c[1].abs_diff(seed[1])).max(c[2].abs_diff(seed[2]));
                    }
                }
            }
            for (s, &d) in state[row + x0..row + x1].iter_mut().zip(dist.iter()) {
                *s = if d <= *tolerance { OPEN } else { WALL };
            }
        }
    }

    /// 某个像素的状态；所在分块还没取样就先取样。
    #[inline]
    fn state_at(&mut self, i: usize) -> u8 {
        if self.state[i] == UNLOADED {
            self.load(i % self.w / T, i / self.w / T);
        }
        self.state[i]
    }

    /// 普通泛洪：与起点四连通的可填像素都标成 IN。返回它们的外接范围。
    fn flood(&mut self, x: usize, y: usize) -> Option<Area> {
        flood_spans(self.w, self.h, x, y, |i| {
            let open = self.state_at(i) == OPEN;
            if open {
                self.state[i] = IN;
            }
            open
        })
    }

    /// 缝隙闭合：从泛洪得到的区域里，去掉要穿过窄口才到得了的部分。`area` 是泛洪结果的外接范围，返回留下部分的。
    ///
    /// 设想一个比缺口略大的球在区域里滚。球心到最近边界（区域外的像素或画布边）的距离叫净空；
    /// 净空不够的地方球心去不了，于是宽度不超过 `gap` 的缺口把"球心去得了的地方"隔成了几片，下面叫高地。
    /// 起点所在的那片高地是要填的。但只填高地会沿着每条边留下一圈窄带，尖角更是整个填不到，
    /// 所以剩下的低处还要分给各片高地：所有高地同时向外漫，先漫净空高的地方、再漫低的，先到先得
    ///（带标记的分水岭）。两片高地在缺口最窄处相遇，口子就封在那里；只挨着一片高地的边角和尖梢全归它，
    /// 一直填到边界。没有缺口时，结果与普通泛洪完全相同。
    ///
    /// 起点自己就在窄处（往高处走也到不了球滚得开的地方）时，把球缩小到那里最宽处的一半。
    /// 这样在一条细长的区域里点一下，填的是这条细长区域，而不是顺着缺口漏到外面的大片空白；
    /// 能封住的缺口也相应变小，只剩明显比这条区域自己还窄的那些。
    ///
    /// 净空用 3-4 倒角距离：横竖一步算 3，斜着一步算 4，与欧氏距离相差几个百分点，前后各扫一遍即可。
    /// n 像素宽的缺口里净空最大是 ceil(n/2) 步，所以 n 为奇数时 n+1 宽的缺口也会被封上。
    fn close_gaps(&mut self, area: Area, sx: usize, sy: usize, gap: u32) -> Area {
        let wide = 3 * gap.min(MAX_GAP).div_ceil(2) as u8;
        // 净空算到比 wide 大就不再往上算，所以高地上的值都是 cap。
        let cap = wide + 1;
        let (bw, bh) = (area.x1 - area.x0, area.y1 - area.y0);
        // 四周留一圈 0（边界），取邻居时不用判断出界。
        let cw = bw + 2;
        let mut cl = vec![0u8; cw * (bh + 2)];
        for y in 0..bh {
            let (src, dst) = ((area.y0 + y) * self.w + area.x0, (y + 1) * cw + 1);
            for x in 0..bw {
                if self.state[src + x] == IN {
                    let i = dst + x;
                    cl[i] = cap.min(cl[i - 1].min(cl[i - cw]) + 3).min(cl[i - cw - 1].min(cl[i - cw + 1]) + 4);
                }
            }
        }
        for i in (cw..cw * (bh + 1)).rev() {
            if cl[i] != 0 {
                cl[i] = cl[i].min(cl[i + 1].min(cl[i + cw]) + 3).min(cl[i + cw - 1].min(cl[i + cw + 1]) + 4);
            }
        }
        let around = |i: usize| [i - 1, i + 1, i - cw, i + cw];

        // 从起点往净空高的地方走，直到上了高地，或者到了一个不够宽的山顶。等高的一片当作平台
        // 整个找一遍，谁旁边有更高的就从那里接着走（MINE 位在这里临时当"找过了"用）。
        let start = (sy - area.y0 + 1) * cw + (sx - area.x0 + 1);
        let mut top = start;
        let mut flat = Vec::new();
        while cl[top] < cap {
            let level = cl[top];
            let mut higher = None;
            flat.clear();
            flat.push(top);
            cl[top] |= MINE;
            let mut head = 0;
            while let (None, Some(&p)) = (higher, flat.get(head)) {
                head += 1;
                for q in around(p) {
                    if cl[q] == level {
                        cl[q] |= MINE;
                        flat.push(q);
                    } else if cl[q] > level && cl[q] & MINE == 0 {
                        higher = Some(q);
                    }
                }
            }
            for &p in &flat {
                cl[p] &= !MINE;
            }
            match higher {
                Some(q) => top = q,
                None => break,
            }
        }
        // 净空不超过 limit 的是低处。到不了高地时，净空过了山顶一半的都当高地。
        let limit = if cl[top] >= cap { wide } else { cl[top] / 2 };

        // 起点所在的高地记到自己名下；别的高地不用标，没有标记的高处就是别人的。
        flood_spans(cw, bh + 2, top % cw, top / cw, |i| {
            let high = cl[i] > limit && cl[i] < THEIRS;
            if high {
                cl[i] |= MINE;
            }
            high
        });

        // 分水岭。每一层净空一个队列，从高到低处理；队列先进先出，所以同一层里各家齐头并进。
        let mut queues = vec![Vec::<u32>::new(); limit as usize + 1];
        if cl[start] <= limit {
            // 起点在低处：直接记到自己名下，免得两家势均力敌时偏偏点的那个像素没填上。
            queues[cl[start] as usize].push(start as u32);
            cl[start] |= MINE;
        }
        for i in cw..cw * (bh + 1) {
            let level = cl[i];
            // 跳过边界、高地和已经有主的（标记位比 limit 大）。
            if level == 0 || level > limit {
                continue;
            }
            let mut owner = 0;
            for q in around(i) {
                if cl[q] & LEVEL > limit {
                    owner |= if cl[q] & MINE != 0 { MINE } else { THEIRS };
                }
            }
            if owner != 0 {
                cl[i] |= if owner & MINE != 0 { MINE } else { THEIRS };
                queues[level as usize].push(i as u32);
            }
        }
        for k in (1..=limit as usize).rev() {
            let mut head = 0;
            while let Some(&p) = queues[k].get(head) {
                head += 1;
                let owner = cl[p as usize] & (MINE | THEIRS);
                for q in around(p as usize) {
                    let level = cl[q];
                    if level != 0 && level <= limit {
                        cl[q] |= owner;
                        // 比当前水位高的没主像素是个小山包（装不下球的鼓包），就地并进来。
                        queues[k.min(level as usize)].push(q as u32);
                    }
                }
            }
            queues[k] = Vec::new();
        }

        let mut kept = Area::pixel(sx, sy);
        for y in 0..bh {
            let (row, src) = ((area.y0 + y) * self.w + area.x0, (y + 1) * cw + 1);
            for x in 0..bw {
                if cl[src + x] & MINE != 0 {
                    kept.include(area.y0 + y, area.x0 + x, area.x0 + x + 1);
                } else if self.state[row + x] == IN {
                    self.state[row + x] = OPEN;
                }
            }
        }
        kept
    }

    /// 软边：把选区延伸到线条抗锯齿的过渡带底下。返回（像素下标，覆盖率）；同一个像素可能出现几次，用时取最大的。
    ///
    /// 抗锯齿线条边上的像素是起点色和线条色按比例混成的，而色差对混色是线性的，所以
    /// "这个像素的色差 ÷ 线芯的色差"就是线条在其中占的份额，余下的份额归填充区域。
    /// 线芯这样找：从区域边上的每个像素出发，朝外沿直线走，只要颜色离起点色越来越远就继续；
    /// 颜色不再变远的前一个像素就是线芯，沿途像素的覆盖率是 1 − 自己的色差 ÷ 线芯的色差。
    /// 线芯自己得 0，所以不会吃进线条；紧挨着区域的就是线芯（硬边）时什么都不加。
    ///
    /// 走得越远才到头，越不像是线条的边：超过 `REACH` 的一半之后覆盖率逐渐打折，到 `REACH` 减为 0。
    /// 这样宽的渐变不会被拖出一条长尾巴，同一条软边也不会因为走向不同而一段有、一段没有。
    fn soften(&mut self, area: Area) -> Vec<(u32, u16)> {
        let (w, h) = (self.w as isize, self.h as isize);
        let mut soft = Vec::new();
        let mut steps = [(0usize, 0u8); REACH];
        for y in area.y0..area.y1 {
            for x in area.x0..area.x1 {
                let i = y * self.w + x;
                if self.state[i] != IN {
                    continue;
                }
                // 上下左右都在区域里的是内部像素，占绝大多数，先筛掉。
                let edge = x == 0 || y == 0 || x + 1 == self.w || y + 1 == self.h;
                if !edge && [i - 1, i + 1, i - self.w, i + self.w].iter().all(|&q| self.state[q] == IN) {
                    continue;
                }
                // 横竖四个方向的线合起来经过过渡带上的每一个像素。斜向是为拐角补的：那里的过渡像素只和区域
                // 对角相邻。斜向的线只经过一半像素，所以只许走 REACH/4 步——这么短的过渡带，横竖方向
                // 最多两倍步数也到头了，还没开始打折，于是相邻像素得到的覆盖率是一致的。
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
                    let reach = if dx != 0 && dy != 0 { REACH / 4 } else { REACH };
                    let (mut qx, mut qy) = (x as isize + dx, y as isize + dy);
                    let (mut n, mut last) = (0, self.tolerance);
                    // 走出画布也算到头。
                    while qx >= 0 && qy >= 0 && qx < w && qy < h {
                        let q = (qy * w + qx) as usize;
                        // 状态不是 WALL 的像素色差在容差之内，比如缝隙另一侧没填到的地方，不属于过渡带。
                        if self.state_at(q) != WALL || self.dist[q] <= last {
                            break;
                        }
                        if n == reach {
                            n = 0;
                            break;
                        }
                        last = self.dist[q];
                        steps[n] = (q, last);
                        n += 1;
                        (qx, qy) = (qx + dx, qy + dy);
                    }
                    let core = last as u32;
                    let trust = ((REACH - n) as u32 * 2 * ONE / REACH as u32).min(ONE);
                    for &(q, d) in &steps[..n.saturating_sub(1)] {
                        soft.push((q as u32, mul15(((core - d as u32) * ONE + core / 2) / core, trust) as u16));
                    }
                }
            }
        }
        soft
    }

    /// 把结果写成选区：`area` 内状态为 IN 的像素全选，`soft` 里的像素按覆盖率部分选中。
    fn into_selection(self, area: Area, soft: &[(u32, u16)]) -> Selection {
        let w = self.w;
        let mut mask = MaskMap::default();
        let mut bounds = area.rect();
        for tc in tiles_in(bounds) {
            let (ox, oy) = (tc.0 as usize * T, tc.1 as usize * T);
            let (x0, x1) = (area.x0.max(ox), area.x1.min(ox + T));
            let rows = area.y0.max(oy)..area.y1.min(oy + T);
            // 没填到的分块不建，选区保持稀疏。
            if !rows.clone().any(|y| self.state[y * w + x0..y * w + x1].contains(&IN)) {
                continue;
            }
            let tile = mask.get_mut(tc);
            for y in rows {
                let out = &mut tile[(y - oy) * T + (x0 - ox)..(y - oy) * T + (x1 - ox)];
                for (o, &s) in out.iter_mut().zip(&self.state[y * w + x0..y * w + x1]) {
                    if s == IN {
                        *o = ONE as u16;
                    }
                }
            }
        }
        for &(i, cover) in soft {
            let (x, y) = (i as usize % w, i as usize / w);
            let slot = &mut mask.get_mut(((x / T) as i32, (y / T) as i32))[y % T * T + x % T];
            *slot = cover.max(*slot);
            bounds = bounds.union(Area::pixel(x, y).rect());
        }
        Selection::from_mask(mask, bounds)
    }
}

/// 扫描线泛洪。`claim(i)` 在像素 i 可填时把它记成已填并返回 true，所以每个像素只会成功一次；
/// 下标按行宽 `w`、共 `h` 行来算。返回填到的范围，起点不可填时返回 None。
///
/// 栈里放的是已经填好的横段，每取出一段就在它上下两行里找相连的新段。
/// 栈的深度与段数而不是像素数相称，也不用递归。
fn flood_spans(w: usize, h: usize, x: usize, y: usize, mut claim: impl FnMut(usize) -> bool) -> Option<Area> {
    // 把已填的像素 (x, row) 向左右扩成一整段。
    fn span(w: usize, row: usize, x: usize, claim: &mut impl FnMut(usize) -> bool) -> (usize, usize) {
        let (mut a, mut b) = (x, x + 1);
        while a > 0 && claim(row + a - 1) {
            a -= 1;
        }
        while b < w && claim(row + b) {
            b += 1;
        }
        (a, b)
    }

    if !claim(y * w + x) {
        return None;
    }
    let mut area = Area::pixel(x, y);
    let (a, b) = span(w, y * w, x, &mut claim);
    let mut stack = vec![(y, a, b)];
    while let Some((y, x0, x1)) = stack.pop() {
        area.include(y, x0, x1);
        // 第 0 行的上一行绕回成一个很大的数，和越过最后一行一样被跳过。
        for ny in [y.wrapping_sub(1), y + 1] {
            if ny >= h {
                continue;
            }
            let mut x = x0;
            while x < x1 {
                if claim(ny * w + x) {
                    let (a, b) = span(w, ny * w, x, &mut claim);
                    stack.push((ny, a, b));
                    // b 处已经知道填不了。
                    x = b + 1;
                } else {
                    x += 1;
                }
            }
        }
    }
    Some(area)
}

/// 图层像素（预乘，15 位）量化成每通道 8 位，仍是预乘的。全透明的像素不管存着什么颜色，都是同一种"透明"。
///
/// 比较颜色时不除回非预乘值：几乎透明的像素除回去之后颜色全是噪声。预乘值之差相当于
/// 按不透明度加权的颜色差，连同不透明度本身的差一起，与两个像素叠在任意底色上看起来的差别相称。
/// 它对混色还是线性的，软边靠这一点估算覆盖率。
#[inline]
fn quantize(px: &[u16]) -> [u8; 4] {
    if px[3] == 0 {
        return [0; 4];
    }
    let q = |v: u16| (((v as u32).min(ONE) * 255 + ONE / 2) >> 15) as u8;
    [q(px[0]), q(px[1]), q(px[2]), q(px[3])]
}

/// 两个像素的色差：各通道（含不透明度）差的最大值。0 表示在 8 位精度下完全相同。
#[inline]
fn apart(a: [u8; 4], b: [u8; 4]) -> u8 {
    a[0].abs_diff(b[0]).max(a[1].abs_diff(b[1])).max(a[2].abs_diff(b[2])).max(a[3].abs_diff(b[3]))
}

#[inline]
fn rgb(v: u32) -> [u8; 3] {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

#[inline]
fn to15(v: u8) -> u32 {
    (v as u32 * ONE + 127) / 255
}

#[inline]
fn mul15(a: u32, b: u32) -> u32 {
    (a * b + ONE / 2) >> 15
}

/// 把颜色 `src`（预乘，不透明度为 `src[3]`）按权重 `a` 以"源覆盖"方式盖到预乘像素上。
#[inline]
fn over(px: [u16; 4], src: [u32; 4], a: u32) -> [u16; 4] {
    let keep = ONE - a;
    let alpha = (mul15(src[3], a) + mul15(px[3] as u32, keep)).min(ONE);
    // 各项分别四舍五入后加起来可能比不透明度多出 1，压回去，保持预乘像素合法。
    let ch = |c: usize| (mul15(src[c], a) + mul15(px[c] as u32, keep)).min(alpha) as u16;
    [ch(0), ch(1), ch(2), alpha as u16]
}

/// 过渡带上的填充：像素里有 `share` 那么多是底色 `seed`，只把这一份底色按权重 `weight` 换成 `src`，
/// 其余（线条）原样留着。结果正好是"线条压在新颜色上"，不剩底色。
///
/// 底色是透明时，这就是把颜色垫到已有笔迹的后面；像素本身就是底色、share 为 1 时，与源覆盖相同。
/// share 是从取样来源估出来的，换到当前图层上未必站得住（比如这里画着别的东西），
/// 所以先按像素自己的数值收紧：拿走这一份底色之后，剩下的必须还是一个合法的预乘像素。
fn replace_share(px: [u16; 4], seed: [u16; 4], src: [u32; 4], share: u32, weight: u32) -> [u16; 4] {
    let (p, s) = (px.map(u32::from), seed.map(u32::from));
    // 每一项限制都是"像素里有 have，每份底色要拿走 per"：份额不能超过 have ÷ per，per 为 0 时不成限制。
    // 三项依次是：不透明度的余量、各通道的颜色、各通道上不透明度比颜色多出的部分。
    let most = |have: u32, per: u32| (have.min(ONE) * ONE).checked_div(per).unwrap_or(ONE);
    let mut share = share.min(most(ONE.saturating_sub(p[3]), ONE.saturating_sub(s[3])));
    for c in 0..3 {
        share = share.min(most(p[c], s[c])).min(most(p[3].saturating_sub(p[c]), s[3].saturating_sub(s[c])));
    }
    let a = mul15(share, weight) as i32;
    let ch = |c: usize| p[c] as i32 + (((src[c] as i32 - s[c] as i32) * a + ONE as i32 / 2) >> 15);
    let alpha = ch(3).clamp(0, ONE as i32);
    [ch(0).clamp(0, alpha) as u16, ch(1).clamp(0, alpha) as u16, ch(2).clamp(0, alpha) as u16, alpha as u16]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Layer, LayerProps};
    use crate::stabilizer::Sample;
    use crate::stroke::{BrushParams, Stroke};

    const WHITE: [u8; 3] = [255, 255, 255];
    const BLACK: [u8; 3] = [0, 0, 0];
    const BLUE: [u8; 3] = [30, 60, 200];
    const ORANGE: [u8; 3] = [230, 120, 20];

    fn opts(tolerance: u8, gap: u32, source: Source) -> FillOptions {
        FillOptions { tolerance, gap, source }
    }

    /// 直接往分块里写一个像素：颜色 c，不透明度 alpha（0 到 1）。
    fn put(tiles: &mut TileMap, x: i32, y: i32, c: [u8; 3], alpha: f32) {
        let a = (alpha * ONE as f32).round() as u32;
        let i = (y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize * 4;
        let t = tiles.get_mut((x.div_euclid(TILE), y.div_euclid(TILE)));
        for (k, &v) in c.iter().enumerate() {
            t.0[i + k] = mul15(to15(v), a) as u16;
        }
        t.0[i + 3] = a as u16;
    }

    fn block(tiles: &mut TileMap, r: Rect, c: [u8; 3], alpha: f32) {
        for y in r.y0..r.y1 {
            for x in r.x0..r.x1 {
                put(tiles, x, y, c, alpha);
            }
        }
    }

    /// 画一个矩形框：r 是外沿，线宽 thick。
    fn frame(tiles: &mut TileMap, r: Rect, thick: i32, c: [u8; 3], alpha: f32) {
        for y in r.y0..r.y1 {
            for x in r.x0..r.x1 {
                if x < r.x0 + thick || x >= r.x1 - thick || y < r.y0 + thick || y >= r.y1 - thick {
                    put(tiles, x, y, c, alpha);
                }
            }
        }
    }

    /// 单层文档，图层内容由 paint 直接写进分块。
    fn doc_with(w: i32, h: i32, paint: impl FnOnce(&mut TileMap)) -> Document {
        let mut layer = Layer::new("线稿");
        paint(&mut layer.tiles);
        Document::from_layers(w, h, WHITE, vec![layer])
    }

    /// 某一层某个像素的不透明度（15 位）。
    fn alpha(doc: &Document, layer: usize, x: i32, y: i32) -> u32 {
        let i = (y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize * 4 + 3;
        doc.layers()[layer].tiles.get((x.div_euclid(TILE), y.div_euclid(TILE))).map_or(0, |t| t.0[i] as u32)
    }

    fn area(r: Rect) -> i32 {
        (r.x1 - r.x0) * (r.y1 - r.y0)
    }

    /// r 里合成后的颜色等于 c 的像素个数。
    fn count(doc: &Document, r: Rect, c: [u8; 3]) -> i32 {
        let rgb = doc.flatten_rgb();
        let at = |x: i32, y: i32| (y * doc.width + x) as usize * 3;
        (r.y0..r.y1).flat_map(|y| (r.x0..r.x1).map(move |x| (x, y))).filter(|&(x, y)| rgb[at(x, y)..at(x, y) + 3] == c).count() as i32
    }

    /// 图层里的预乘像素必须合法：颜色不超过不透明度，不透明度不超过 1。
    fn assert_valid(doc: &Document) {
        for layer in doc.layers() {
            for tc in layer.tiles.coords() {
                for px in layer.tiles.get(tc).unwrap().0.as_chunks::<4>().0 {
                    assert!(px[3] as u32 <= ONE && px[..3].iter().all(|&c| c <= px[3]), "{px:?}");
                }
            }
        }
    }

    #[test]
    fn plain_fill_covers_the_whole_canvas_as_one_undo_step() {
        // 画布尺寸不是分块边长的整数倍。
        let mut doc = Document::new(150, 100);
        let dirty = bucket_fill(&mut doc, 70, 40, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
        assert_eq!(dirty, doc.bounds());
        assert_eq!(count(&doc, doc.bounds(), BLUE), 150 * 100);
        // 边上的分块伸到画布外面的部分不填。
        assert_eq!((alpha(&doc, 0, 149, 99), alpha(&doc, 0, 150, 50), alpha(&doc, 0, 100, 100)), (ONE, 0, 0));
        assert_eq!(doc.undo_depth(), 1);
        doc.undo();
        assert_eq!(count(&doc, doc.bounds(), WHITE), 150 * 100);
        assert!(!doc.can_undo());
        doc.redo();
        assert_eq!(doc.pick(149, 99), Some(BLUE));
    }

    #[test]
    fn a_closed_outline_stops_the_fill() {
        let inside = Rect { x0: 43, y0: 33, x1: 137, y1: 107 };
        let mut doc = doc_with(200, 150, |t| frame(t, Rect { x0: 40, y0: 30, x1: 140, y1: 110 }, 3, BLACK, 1.0));
        let outline = count(&doc, doc.bounds(), BLACK);
        // 从里面填：填满到线边，线和外面不动。
        let dirty = bucket_fill(&mut doc, 90, 70, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
        assert_eq!(dirty, inside);
        assert_eq!(count(&doc, inside, BLUE), area(inside));
        assert_eq!(count(&doc, doc.bounds(), BLUE), area(inside));
        assert_eq!(count(&doc, doc.bounds(), BLACK), outline);
        // 从外面填：里面不动。
        doc.undo();
        bucket_fill(&mut doc, 10, 10, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
        assert_eq!(count(&doc, inside, WHITE), area(inside));
        assert_eq!(count(&doc, doc.bounds(), BLUE), 200 * 150 - area(inside) - outline);
        assert_eq!(count(&doc, doc.bounds(), BLACK), outline);
    }

    #[test]
    fn tolerance_decides_whether_a_similar_colour_is_included() {
        // 左半边灰 100，右半边在一个通道上差 10。
        let doc = doc_with(128, 64, |t| {
            block(t, Rect { x0: 0, y0: 0, x1: 64, y1: 64 }, [100, 100, 100], 1.0);
            block(t, Rect { x0: 64, y0: 0, x1: 128, y1: 64 }, [100, 110, 104], 1.0);
        });
        for source in [Source::Layer(0), Source::Merged] {
            for (tolerance, both) in [(0, false), (9, false), (10, true), (255, true)] {
                let s = flood_select(&doc, 5, 5, &opts(tolerance, 0, source));
                assert_eq!(s.get(5, 5) as u32, ONE);
                assert_eq!(s.get(63, 60) as u32, ONE);
                let right = [s.get(64, 30), s.get(100, 30), s.get(127, 63)].map(u32::from);
                assert_eq!(right, if both { [ONE; 3] } else { [0; 3] }, "{source:?} 容差 {tolerance}");
            }
        }
    }

    #[test]
    fn layer_comparison_counts_alpha_and_ignores_the_colour_of_transparent_pixels() {
        let doc = doc_with(192, 64, |t| {
            // 中间一块半透明的黑；右边一块全透明、但颜色通道里留着垃圾值。
            block(t, Rect { x0: 64, y0: 0, x1: 128, y1: 64 }, BLACK, 0.5);
            for px in t.get_mut((2, 0)).0.as_chunks_mut::<4>().0 {
                *px = [30000, 20000, 10000, 0];
            }
        });
        let layer = |tolerance| flood_select(&doc, 150, 30, &opts(tolerance, 0, Source::Layer(0)));
        // 起点在留着垃圾值的透明处：它和左边没画过的透明处是同一种颜色，但到不了那里，中间隔着半透明的黑。
        assert_eq!(layer(0).bounds(), Rect { x0: 128, y0: 0, x1: 192, y1: 64 });
        assert_eq!(layer(127).bounds(), Rect { x0: 128, y0: 0, x1: 192, y1: 64 });
        // 半透明黑与透明只差在不透明度上：0.5 就是 128。
        assert_eq!(layer(128).bounds(), doc.bounds());
    }

    /// 带抗锯齿边的黑框：线芯 3 像素不透明，里外各有两圈过渡（不透明度 2/3、1/3）。
    /// 左边线的线芯在 x = 40..43，过渡在 43、44，内部从 45 开始；上边线同理，内部从 y = 35 开始。
    fn soft_frame(tiles: &mut TileMap) {
        let grow = |d: i32| Rect { x0: 40 - d, y0: 30 - d, x1: 160 + d, y1: 120 + d };
        frame(tiles, grow(2), 7, BLACK, 1.0 / 3.0);
        frame(tiles, grow(1), 5, BLACK, 2.0 / 3.0);
        frame(tiles, grow(0), 3, BLACK, 1.0);
    }

    /// 沿 y = 75 从填充内部走到左边线的线芯：合成后的颜色每个通道都只能单调地从填充色变到线条色，
    /// 中间不许有比两头都浅的像素；过渡带上应当正好是"线条压在填充色上"。拐角处的过渡像素同样要求。
    fn assert_no_halo(doc: &Document, fill: [u8; 3]) {
        let line: Vec<[u8; 3]> = (40..=60).rev().map(|x| doc.pick(x, 75).unwrap()).collect();
        assert_eq!((line[0], line[20]), (fill, BLACK), "{line:?}");
        for pair in line.windows(2) {
            assert!((0..3).all(|c| pair[1][c] <= pair[0][c]), "{line:?}");
        }
        let expect = |x: i32, y: i32, keep: f32| {
            let got = doc.pick(x, y).unwrap();
            assert!((0..3).all(|c| (got[c] as f32 - fill[c] as f32 * keep).abs() <= 1.5), "({x}, {y}): {got:?}");
        };
        expect(45, 75, 1.0);
        expect(44, 75, 2.0 / 3.0);
        expect(43, 75, 1.0 / 3.0);
        expect(42, 75, 0.0);
        expect(44, 34, 2.0 / 3.0);
        expect(43, 33, 1.0 / 3.0);
        // 线的外侧没有动过。
        assert_eq!(doc.pick(38, 75), Some([170, 170, 170]));
        assert_eq!(doc.pick(30, 75), Some(WHITE));
    }

    #[test]
    fn magic_wand_reaches_under_the_antialiased_fringe() {
        let doc = doc_with(200, 150, soft_frame);
        for source in [Source::Layer(0), Source::Merged] {
            let s = flood_select(&doc, 100, 75, &opts(0, 0, source));
            let cover = |x: i32, y: i32| s.get(x, y) as f32 / ONE as f32;
            assert_eq!(cover(45, 75), 1.0);
            assert!((cover(44, 75) - 2.0 / 3.0).abs() < 0.01 && (cover(43, 75) - 1.0 / 3.0).abs() < 0.01, "{source:?}");
            assert!((cover(44, 34) - 2.0 / 3.0).abs() < 0.01 && (cover(43, 33) - 1.0 / 3.0).abs() < 0.01, "{source:?}");
            // 线芯和线的另一侧不选。
            assert_eq!((cover(42, 75), cover(40, 75), cover(39, 75), cover(38, 75)), (0.0, 0.0, 0.0, 0.0));
            assert_eq!(s.bounds(), Rect { x0: 43, y0: 33, x1: 157, y1: 117 });
        }
    }

    #[test]
    fn filling_next_to_an_antialiased_line_leaves_no_halo() {
        // 填在线稿自己这一层上。
        for source in [Source::Layer(0), Source::Merged] {
            let mut doc = doc_with(200, 150, soft_frame);
            bucket_fill(&mut doc, 100, 75, BLUE, 1.0, &opts(0, 0, source));
            assert_no_halo(&doc, BLUE);
            assert_valid(&doc);
            // 过渡带上线条还在，像素变成了不透明的；线芯没有被吃掉。
            assert_eq!((alpha(&doc, 0, 44, 75), alpha(&doc, 0, 43, 75)), (ONE, ONE));
            assert_eq!(doc.pick(41, 75), Some(BLACK));
            // 换一种颜色再填一次：过渡带里的旧颜色也跟着换掉，不留一圈蓝边。
            bucket_fill(&mut doc, 100, 75, ORANGE, 1.0, &opts(0, 0, source));
            assert_no_halo(&doc, ORANGE);
            assert_valid(&doc);
        }
        // 线稿在上，填在它下面的空图层上：颜色铺满过渡带底下，到线芯为止。
        let mut lines = Layer::new("线稿");
        soft_frame(&mut lines.tiles);
        let mut doc = Document::from_layers(200, 150, WHITE, vec![Layer::new("颜色"), lines]);
        doc.set_active(0);
        bucket_fill(&mut doc, 100, 75, BLUE, 1.0, &opts(0, 0, Source::Merged));
        assert_no_halo(&doc, BLUE);
        assert_valid(&doc);
        assert_eq!((alpha(&doc, 0, 44, 75), alpha(&doc, 0, 43, 75), alpha(&doc, 0, 42, 75)), (ONE, ONE, 0));
        assert_eq!((alpha(&doc, 1, 44, 75), alpha(&doc, 1, 43, 75)), (10923, 21845), "线稿层不动");
    }

    const FRAME: Rect = Rect { x0: 50, y0: 40, x1: 250, y1: 160 };
    const ROOM: Rect = Rect { x0: 53, y0: 43, x1: 247, y1: 157 };
    const ABOVE: Rect = Rect { x0: 0, y0: 0, x1: 300, y1: 40 };

    /// 300×200 的画布上一个 3 像素粗的黑框，上边线从 x = 150 起开一个 width 像素宽的口子。
    fn gapped(width: i32, more: impl FnOnce(&mut TileMap)) -> Document {
        doc_with(300, 200, |t| {
            frame(t, FRAME, 3, BLACK, 1.0);
            block(t, Rect { x0: 150, y0: 40, x1: 150 + width, y1: 43 }, BLACK, 0.0);
            more(t);
        })
    }

    #[test]
    fn gap_closing_keeps_the_fill_inside_a_small_opening() {
        let fill = |width: i32, gap: u32| {
            let mut doc = gapped(width, |_| {});
            bucket_fill(&mut doc, 150, 100, BLUE, 1.0, &opts(0, gap, Source::Layer(0)));
            doc
        };
        // 不闭合：从 2 像素的口子漏出去，里外都填上了。
        let doc = fill(2, 0);
        assert_eq!(count(&doc, ROOM, BLUE), area(ROOM));
        assert_eq!(count(&doc, ABOVE, BLUE), area(ABOVE));
        // 闭合 3 像素：不漏。里面一直填到线边和四个角，没有留一圈空（任务要求至少 97%）。
        let doc = fill(2, 3);
        assert_eq!(count(&doc, ROOM, BLUE), area(ROOM));
        assert_eq!(count(&doc, ABOVE, BLUE), 0);
        // 漏出框外的只可能是口子里的那几个像素。
        assert!(count(&doc, doc.bounds(), BLUE) - area(ROOM) <= 6);
        // 12 像素的口子比设定的宽，照样漏。
        let doc = fill(12, 3);
        assert_eq!(count(&doc, ROOM, BLUE), area(ROOM));
        assert_eq!(count(&doc, ABOVE, BLUE), area(ABOVE));
    }

    #[test]
    fn gap_closing_works_wherever_the_seed_is() {
        // 贴着线、在角上、在口子正下方点，结果都一样。
        for (x, y) in [(53, 43), (246, 156), (150, 43), (151, 44), (60, 100)] {
            let mut doc = gapped(2, |_| {});
            bucket_fill(&mut doc, x, y, BLUE, 1.0, &opts(0, 3, Source::Layer(0)));
            assert_eq!((count(&doc, ROOM, BLUE), count(&doc, ABOVE, BLUE)), (area(ROOM), 0), "起点 ({x}, {y})");
        }
        // 点在口子里面：填哪一边都说得通，但不能两边都填。
        let mut doc = gapped(2, |_| {});
        bucket_fill(&mut doc, 150, 41, BLUE, 1.0, &opts(0, 3, Source::Layer(0)));
        assert_eq!(doc.pick(150, 41), Some(BLUE));
        assert!(count(&doc, ROOM, BLUE) == 0 || count(&doc, ABOVE, BLUE) == 0);
        // 从外面点：外面填满，不漏进框里。
        let mut doc = gapped(2, |_| {});
        bucket_fill(&mut doc, 5, 5, BLUE, 1.0, &opts(0, 3, Source::Layer(0)));
        assert_eq!((count(&doc, ROOM, BLUE), count(&doc, ABOVE, BLUE)), (0, area(ABOVE)));
    }

    #[test]
    fn gap_closing_fills_narrow_places_without_leaking() {
        // 框里放一个实心块，只留一圈 3 像素宽的走廊，走廊通过 2 像素的口子连着外面。
        // 球滚不进走廊，但在走廊里点，填的应当是走廊。
        let core = Rect { x0: 56, y0: 46, x1: 244, y1: 154 };
        let mut doc = gapped(2, |t| block(t, core, BLACK, 1.0));
        bucket_fill(&mut doc, 100, 44, BLUE, 1.0, &opts(0, 3, Source::Layer(0)));
        assert_eq!(count(&doc, ROOM, BLUE), area(ROOM) - area(core));
        assert_eq!(count(&doc, ABOVE, BLUE), 0);
        // 走廊有两段宽一些（5 像素），缝隙宽度又设得比走廊还宽：从窄处、宽处点，都是整条走廊，都不漏。
        let wider = [Rect { x0: 80, y0: 46, x1: 120, y1: 48 }, Rect { x0: 180, y0: 152, x1: 220, y1: 154 }];
        for (x, y) in [(60, 44), (100, 45), (240, 155), (200, 152)] {
            let mut doc = gapped(2, |t| {
                block(t, core, BLACK, 1.0);
                wider.iter().for_each(|&r| block(t, r, BLACK, 0.0));
            });
            bucket_fill(&mut doc, x, y, BLUE, 1.0, &opts(0, 10, Source::Layer(0)));
            assert_eq!(count(&doc, ROOM, BLUE), area(ROOM) - area(core) + 2 * area(wider[0]), "起点 ({x}, {y})");
            assert_eq!(count(&doc, ABOVE, BLUE), 0);
        }

        // 房间右下角伸出一条带瓶颈的细尾巴（3 像素宽，中间一段只有 1 像素），整条都要填上。
        let tail = |t: &mut TileMap| {
            block(t, Rect { x0: 247, y0: 150, x1: 290, y1: 153 }, BLACK, 0.0);
            frame(t, Rect { x0: 250, y0: 147, x1: 293, y1: 156 }, 3, BLACK, 1.0);
            block(t, Rect { x0: 247, y0: 150, x1: 253, y1: 153 }, BLACK, 0.0);
            block(t, Rect { x0: 265, y0: 150, x1: 268, y1: 151 }, BLACK, 1.0);
            block(t, Rect { x0: 265, y0: 152, x1: 268, y1: 153 }, BLACK, 1.0);
        };
        let mut doc = gapped(2, tail);
        bucket_fill(&mut doc, 150, 100, BLUE, 1.0, &opts(0, 3, Source::Layer(0)));
        let tail_area = Rect { x0: 247, y0: 150, x1: 290, y1: 153 };
        assert_eq!(count(&doc, tail_area, BLUE), area(tail_area) - 6);
        assert_eq!(count(&doc, ROOM, BLUE), area(ROOM));
        assert_eq!(count(&doc, ABOVE, BLUE), 0);
    }

    #[test]
    fn gap_closing_handles_a_diagonal_opening() {
        // 菱形框（斜线），右上边开一个约 2 像素宽的口子。
        let at = |x: i32, y: i32| ((x - 150).abs() + (y - 100).abs(), x - 150, 100 - y);
        let doc_with_diamond = || {
            doc_with(300, 200, |t| {
                for y in 0..200 {
                    for x in 0..300 {
                        let (ring, a, b) = at(x, y);
                        if (76..80).contains(&ring) && !(a > 0 && b > 0 && (0..3).contains(&(a - b))) {
                            put(t, x, y, BLACK, 1.0);
                        }
                    }
                }
            })
        };
        let inside = |doc: &Document| {
            let rgb = doc.flatten_rgb();
            let cells = (0..200).flat_map(|y| (0..300).map(move |x| (x, y))).filter(|&(x, y)| at(x, y).0 < 76);
            cells.map(|(x, y)| (rgb[(y * 300 + x) as usize * 3..][..3] == BLUE) as i32).sum::<i32>()
        };
        let total = 2 * 76 * 75 + 1;
        let mut doc = doc_with_diamond();
        bucket_fill(&mut doc, 150, 100, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
        assert_eq!((inside(&doc), doc.pick(5, 5)), (total, Some(BLUE)), "不闭合时漏出去");
        let mut doc = doc_with_diamond();
        bucket_fill(&mut doc, 150, 100, BLUE, 1.0, &opts(0, 3, Source::Layer(0)));
        assert_eq!((inside(&doc), doc.pick(5, 5)), (total, Some(WHITE)));
        assert!(count(&doc, doc.bounds(), BLUE) - total <= 8);
    }

    #[test]
    fn merged_source_sees_other_layers_and_layer_source_does_not() {
        let inside = Rect { x0: 43, y0: 33, x1: 137, y1: 107 };
        let mut doc = doc_with(200, 150, |t| frame(t, Rect { x0: 40, y0: 30, x1: 140, y1: 110 }, 3, BLACK, 1.0));
        doc.add_layer();
        assert_eq!(doc.active(), 1);
        // 看合成画面：下面那层的线框挡得住。
        bucket_fill(&mut doc, 90, 70, BLUE, 1.0, &opts(0, 0, Source::Merged));
        assert_eq!(count(&doc, doc.bounds(), BLUE), area(inside));
        assert_eq!(count(&doc, inside, BLUE), area(inside));
        assert_eq!((alpha(&doc, 1, 90, 70), alpha(&doc, 0, 90, 70)), (ONE, 0), "填在当前图层上");
        // 只看当前这个空图层：到处都一样，整张画布填满，连线框也盖住了。
        doc.undo();
        bucket_fill(&mut doc, 90, 70, BLUE, 1.0, &opts(0, 0, Source::Layer(1)));
        assert_eq!(count(&doc, doc.bounds(), BLUE), 200 * 150);
        // 只看线稿层、填在当前图层上：同样被线框挡住。
        doc.undo();
        bucket_fill(&mut doc, 90, 70, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
        assert_eq!(count(&doc, doc.bounds(), BLUE), area(inside));
    }

    #[test]
    fn the_document_selection_clips_the_fill() {
        let chosen = Rect { x0: 30, y0: 20, x1: 90, y1: 70 };
        let mut doc = Document::new(200, 150);
        doc.set_selection(Some(Selection::rect(chosen)));
        // 起点不必在选区里：填的是"相连的区域"与选区的交集。
        let dirty = bucket_fill(&mut doc, 150, 100, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
        assert_eq!(count(&doc, doc.bounds(), BLUE), area(chosen));
        assert_eq!(count(&doc, chosen, BLUE), area(chosen));
        assert_eq!(dirty.intersect(chosen), chosen);
        assert_eq!(doc.undo_depth(), 1);

        // 半选的地方按比例填。
        let mut half = MaskMap::default();
        half.get_mut((0, 0)).fill(ONE as u16 / 2);
        let mut doc = Document::new(200, 150);
        doc.set_selection(Some(Selection::from_mask(half, Rect { x0: 0, y0: 0, x1: 64, y1: 64 })));
        bucket_fill(&mut doc, 150, 100, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
        assert_eq!((alpha(&doc, 0, 10, 10), alpha(&doc, 0, 70, 10)), (ONE / 2, 0));
        assert_valid(&doc);

        // 选区和要填的区域没有交集：什么都不做，也不留撤销步骤。
        let mut doc = doc_with(200, 150, |t| frame(t, Rect { x0: 40, y0: 30, x1: 140, y1: 110 }, 3, BLACK, 1.0));
        doc.set_selection(Some(Selection::rect(Rect { x0: 150, y0: 0, x1: 200, y1: 150 })));
        assert_eq!(bucket_fill(&mut doc, 90, 70, BLUE, 1.0, &opts(0, 0, Source::Layer(0))), Rect::EMPTY);
        assert_eq!(doc.undo_depth(), 0);
    }

    #[test]
    fn opacity_blends_over_what_is_already_there() {
        let mut doc = doc_with(100, 80, |t| block(t, Rect { x0: 0, y0: 0, x1: 100, y1: 80 }, [200, 100, 0], 1.0));
        bucket_fill(&mut doc, 50, 40, [0, 100, 200], 0.5, &opts(0, 0, Source::Layer(0)));
        let c = doc.pick(50, 40).unwrap();
        assert!((0..3).all(|k| (c[k] as i32 - 100).abs() <= 1), "{c:?}");
        assert_eq!(alpha(&doc, 0, 50, 40), ONE);
        assert_valid(&doc);
        // 透明图层上半透明地填两次：不透明度按源覆盖累加。
        let mut doc = Document::new(100, 80);
        bucket_fill(&mut doc, 50, 40, BLUE, 0.5, &opts(0, 0, Source::Layer(0)));
        assert_eq!(alpha(&doc, 0, 99, 79), ONE / 2);
        bucket_fill(&mut doc, 50, 40, WHITE, 0.5, &opts(0, 0, Source::Layer(0)));
        assert_eq!(alpha(&doc, 0, 99, 79), ONE * 3 / 4);
        assert_valid(&doc);
        assert_eq!(doc.undo_depth(), 2);
        // 填上去和原来一模一样：不算改动。
        let mut doc = Document::new(100, 80);
        bucket_fill(&mut doc, 50, 40, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
        assert_eq!(bucket_fill(&mut doc, 50, 40, BLUE, 1.0, &opts(0, 0, Source::Layer(0))), Rect::EMPTY);
        assert_eq!(doc.undo_depth(), 1);
    }

    #[test]
    fn locked_layers_and_protected_alpha_are_respected() {
        let set = |doc: &mut Document, change: fn(&mut LayerProps)| {
            let mut props = doc.layers()[0].props.clone();
            change(&mut props);
            doc.set_props_live(0, props);
        };
        let layer = opts(0, 0, Source::Layer(0));
        // 锁定的图层填不了。
        let mut doc = Document::new(100, 80);
        set(&mut doc, |p| p.locked = true);
        assert_eq!(bucket_fill(&mut doc, 50, 40, BLUE, 1.0, &layer), Rect::EMPTY);
        assert_eq!((alpha(&doc, 0, 50, 40), doc.undo_depth()), (0, 0));

        // 保护不透明度：左边一块不透明的灰，中间一块半透明的灰，右边透明。
        let grey = [100, 100, 100];
        let mut doc = doc_with(100, 80, |t| {
            block(t, Rect { x0: 0, y0: 0, x1: 40, y1: 80 }, grey, 1.0);
            block(t, Rect { x0: 40, y0: 0, x1: 70, y1: 80 }, grey, 0.5);
        });
        set(&mut doc, |p| p.lock_alpha = true);
        let alphas = |doc: &Document| (0..100).map(|x| alpha(doc, 0, x, 40)).collect::<Vec<_>>();
        let before = alphas(&doc);
        // 透明的地方填不上，也不留撤销步骤。
        assert_eq!(bucket_fill(&mut doc, 90, 40, BLUE, 1.0, &layer), Rect::EMPTY);
        assert_eq!(doc.undo_depth(), 0);
        // 有颜色的地方换了颜色，不透明度一丝没变。
        assert_eq!(bucket_fill(&mut doc, 10, 40, BLUE, 1.0, &layer), Rect { x0: 0, y0: 0, x1: 40, y1: 80 });
        assert_eq!(doc.pick(10, 40), Some(BLUE));
        bucket_fill(&mut doc, 50, 40, BLUE, 0.5, &layer);
        // 半透明那块：颜色是灰和蓝各一半，再以一半的不透明度叠在白底上。
        let c = doc.pick(50, 40).unwrap();
        assert!((0..3).all(|k| (c[k] as f32 - (0.25 * grey[k] as f32 + 0.25 * BLUE[k] as f32 + 127.5)).abs() <= 1.5), "{c:?}");
        assert_eq!(alphas(&doc), before);
        assert_valid(&doc);

        // 线框里面是透明的：保护不透明度时什么都填不上，过渡带也不动。
        let mut doc = doc_with(200, 150, soft_frame);
        set(&mut doc, |p| p.lock_alpha = true);
        assert_eq!(bucket_fill(&mut doc, 100, 75, BLUE, 1.0, &layer), Rect::EMPTY);
        assert_eq!(doc.undo_depth(), 0);
    }

    #[test]
    fn odd_inputs_give_an_empty_result_instead_of_a_panic() {
        let mut doc = Document::new(100, 80);
        let any = opts(0, 0, Source::Layer(0));
        for (x, y) in [(-1, 0), (0, -1), (100, 0), (0, 80), (i32::MIN, i32::MAX), (i32::MAX, 5)] {
            assert!(flood_select(&doc, x, y, &any).is_empty());
            assert_eq!(bucket_fill(&mut doc, x, y, BLUE, 1.0, &any), Rect::EMPTY);
        }
        // 取样的图层不存在。
        assert!(flood_select(&doc, 5, 5, &opts(0, 0, Source::Layer(7))).is_empty());
        assert_eq!(bucket_fill(&mut doc, 5, 5, BLUE, 1.0, &opts(0, 3, Source::Layer(usize::MAX))), Rect::EMPTY);
        // 不透明度为 0、负数或不是数。
        for opacity in [0.0, -3.0, f32::NAN] {
            assert_eq!(bucket_fill(&mut doc, 5, 5, BLUE, opacity, &any), Rect::EMPTY);
        }
        assert_eq!(doc.undo_depth(), 0);
        // 不透明度超过 1 当作 1；缝隙宽度大得离谱也照常工作。
        assert_eq!(bucket_fill(&mut doc, 5, 5, BLUE, 9.0, &opts(255, u32::MAX, Source::Merged)), doc.bounds());
        assert_eq!(count(&doc, doc.bounds(), BLUE), 100 * 80);
        // 最小的画布。
        let mut tiny = Document::new(1, 1);
        assert_eq!(bucket_fill(&mut tiny, 0, 0, BLUE, 1.0, &opts(0, 5, Source::Merged)), tiny.bounds());
        assert_eq!(tiny.pick(0, 0), Some(BLUE));
    }



    /// 用真的笔刷以 (200, 200) 为圆心画一段半径 120 的圆弧（角度制）。
    fn arc(doc: &mut Document, params: BrushParams, from: f32, to: f32) {
        let at = |deg: f32| Sample { x: 200.0 + 120.0 * deg.to_radians().cos(), y: 200.0 + 120.0 * deg.to_radians().sin(), p: 1.0 };
        let (mut stroke, _) = Stroke::begin(params, 0, at(from));
        let mut deg = from;
        while deg < to {
            deg = (deg + 3.0).min(to);
            stroke.add(at(deg));
        }
        stroke.end();
        doc.commit_stroke(&stroke);
    }

    /// 合成画面里，圆内（半径 116 以内）和圆外（半径 125 以外）颜色正好是 c 的像素个数。
    fn in_and_out(doc: &Document, c: [u8; 3]) -> (usize, usize) {
        let rgb = doc.flatten_rgb();
        let hits = |keep: &dyn Fn(i32) -> bool| {
            let cells = (0..400i32).flat_map(|y| (0..400i32).map(move |x| (x, y)));
            cells.filter(|&(x, y)| keep((x - 200).pow(2) + (y - 200).pow(2)) && rgb[(y * 400 + x) as usize * 3..][..3] == c).count()
        };
        (hits(&|d2| d2 < 116 * 116), hits(&|d2| d2 > 125 * 125))
    }

    /// 笔刷引擎画出来的圆（各个方向的抗锯齿边都有），填进去之后沿每条半径从里往外看到线最黑的地方，
    /// 每个通道都只能越来越暗：填充和线之间没有浅边。
    #[test]
    fn a_brush_drawn_outline_fills_without_a_halo_in_any_direction() {
        let fill = [20, 40, 120];
        // （笔刷直径，硬度，线稿是否单独放在上面一层）
        for (size, hardness, lines_above) in [(6.0, 1.0, false), (6.0, 1.0, true), (14.0, 0.5, false)] {
            for source in [Source::Layer(lines_above as usize), Source::Merged] {
                let mut doc = Document::new(400, 400);
                if lines_above {
                    doc.add_layer();
                }
                arc(&mut doc, BrushParams { size, hardness, ..Default::default() }, 0.0, 360.0);
                doc.set_active(0);
                let before = doc.flatten_rgb();
                bucket_fill(&mut doc, 200, 200, fill, 1.0, &opts(0, 0, source));
                assert_valid(&doc);
                let after = doc.flatten_rgb();
                for step in 0..720 {
                    let (sin, cos) = (step as f32 * 0.5).to_radians().sin_cos();
                    let at = |r: i32| ((200.0 + r as f32 * sin) as usize * 400 + (200.0 + r as f32 * cos) as usize) * 3;
                    let darkest = (80..135).min_by_key(|&r| before[at(r)]).unwrap();
                    assert_eq!(after[at(80)..][..3], fill);
                    for r in 80..darkest {
                        let (a, b) = (&after[at(r)..][..3], &after[at(r + 1)..][..3]);
                        assert!((0..3).all(|c| b[c] <= a[c]), "直径 {size} 硬度 {hardness} {source:?}，第 {step} 条半径，r = {r}：{a:?} 之后是 {b:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn gap_closing_works_on_a_brush_drawn_outline() {
        let closed = {
            let mut doc = Document::new(400, 400);
            arc(&mut doc, BrushParams { size: 5.0, ..Default::default() }, 0.0, 360.0);
            bucket_fill(&mut doc, 200, 200, BLUE, 1.0, &opts(0, 0, Source::Layer(0)));
            in_and_out(&doc, BLUE)
        };
        assert!(closed.0 > 42_000 && closed.1 == 0, "{closed:?}");
        // 圆弧两头之间留一个 4 像素左右的口子（两个圆头最近处）。
        let open = |gap: u32| {
            let mut doc = Document::new(400, 400);
            arc(&mut doc, BrushParams { size: 5.0, ..Default::default() }, 0.0, 355.7);
            bucket_fill(&mut doc, 200, 200, BLUE, 1.0, &opts(0, gap, Source::Layer(0)));
            in_and_out(&doc, BLUE)
        };
        assert!(open(0).1 > 100_000, "不闭合时漏到外面");
        // 闭合之后，圆里面填到的和没有口子时一样多。
        assert_eq!(open(4), closed);
        assert_eq!(open(8), closed);
    }

    /// 随便乱画、随便点：不崩溃；缝隙闭合的结果是普通结果的一部分，包含起点，而且连成一片；
    /// 填完之后图层里的像素仍然合法，撤销步骤数对得上。
    #[test]
    fn random_scribbles_keep_the_invariants() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = move |n: i32| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 20) % n as u64) as i32
        };
        for round in 0..300 {
            let (w, h) = (20 + rnd(120), 20 + rnd(100));
            let mut layer = Layer::new("乱画");
            for _ in 0..rnd(8) {
                let (x0, y0) = (rnd(w), rnd(h));
                let r = Rect { x0, y0, x1: (x0 + 3 + rnd(60)).min(w), y1: (y0 + 3 + rnd(60)).min(h) };
                let c = [BLACK, [200, 30, 30], [250, 250, 250]][rnd(3) as usize];
                let alpha = [1.0, 1.0, 0.6, 0.2][rnd(4) as usize];
                match rnd(3) {
                    0 => block(&mut layer.tiles, r, c, alpha),
                    kind => {
                        frame(&mut layer.tiles, r, 1 + rnd(3), c, alpha);
                        if kind == 2 {
                            // 上边线开个口子。
                            let x = r.x0 + rnd(r.x1 - r.x0);
                            block(&mut layer.tiles, Rect { x0: x, y0: r.y0, x1: (x + 1 + rnd(5)).min(w), y1: (r.y0 + 4).min(h) }, BLACK, 0.0);
                        }
                    }
                }
            }
            let mut doc = Document::from_layers(w, h, WHITE, vec![layer, Layer::new("上面一层")]);
            doc.set_active(rnd(2) as usize);
            let (x, y) = (rnd(w), rnd(h));
            let tolerance = [0, 0, 30, 200][rnd(4) as usize];
            let gap = [1, 2, 3, 5, 8, 40][rnd(6) as usize];
            let source = [Source::Layer(0), Source::Merged][rnd(2) as usize];
            let plain = flood_select(&doc, x, y, &opts(tolerance, 0, source));
            let closed = flood_select(&doc, x, y, &opts(tolerance, gap, source));
            let full = |s: &Selection, x: i32, y: i32| s.get(x, y) as u32 == ONE;
            assert!(full(&plain, x, y) && full(&closed, x, y), "第 {round} 轮：起点没选上");
            let mut total = 0;
            for py in -2..h + 2 {
                for px in -2..w + 2 {
                    for s in [&plain, &closed] {
                        let inside = |r: Rect| px >= r.x0 && px < r.x1 && py >= r.y0 && py < r.y1;
                        assert!(s.get(px, py) == 0 || (inside(s.bounds()) && inside(doc.bounds())), "第 {round} 轮：选到了范围外");
                    }
                    assert!(full(&plain, px, py) || !full(&closed, px, py), "第 {round} 轮：闭合后反而多了");
                    total += full(&closed, px, py) as usize;
                }
            }
            let mut seen = vec![false; (w * h) as usize];
            let mut stack = vec![(x, y)];
            let mut reached = 0;
            while let Some((px, py)) = stack.pop() {
                if full(&closed, px, py) && !std::mem::replace(&mut seen[(py * w + px) as usize], true) {
                    reached += 1;
                    stack.extend([(px + 1, py), (px - 1, py), (px, py + 1), (px, py - 1)]);
                }
            }
            assert_eq!(reached, total, "第 {round} 轮：结果不连通");

            // 填两次：第二次遇到的是第一次留下的过渡像素。
            for opacity in [[1.0, 0.5][rnd(2) as usize], 1.0] {
                let depth = doc.undo_depth();
                let color = [rnd(256) as u8, rnd(256) as u8, rnd(256) as u8];
                let dirty = bucket_fill(&mut doc, x, y, color, opacity, &opts(tolerance, [0, gap][rnd(2) as usize], source));
                assert_valid(&doc);
                assert_eq!(dirty.intersect(doc.bounds()), dirty);
                assert_eq!(doc.undo_depth(), depth + !dirty.is_empty() as usize);
            }
        }
    }

    /// 3000×2000、基本空着的画布点一下填满。发布构建下只要几十毫秒；这里防的是退化到按秒计。
    #[test]
    fn a_large_canvas_fills_in_well_under_a_second() {
        let cases = [("单层", opts(0, 0, Source::Layer(0))), ("合成", opts(0, 0, Source::Merged)), ("缝隙闭合", opts(0, 8, Source::Layer(0)))];
        for (name, o) in cases {
            let mut doc = doc_with(3000, 2000, |t| frame(t, Rect { x0: 100, y0: 100, x1: 900, y1: 700 }, 3, BLACK, 1.0));
            let start = std::time::Instant::now();
            let dirty = bucket_fill(&mut doc, 1500, 1000, BLUE, 1.0, &o);
            let took = start.elapsed();
            eprintln!("3000×2000 {name}: {took:?}");
            assert_eq!(dirty, doc.bounds());
            assert_eq!((alpha(&doc, 0, 0, 0), alpha(&doc, 0, 2999, 1999), alpha(&doc, 0, 500, 400)), (ONE, ONE, 0));
            assert!(took.as_millis() < 1000, "{name}: {took:?}");
        }
    }
}
