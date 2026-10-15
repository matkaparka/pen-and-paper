// SPDX-License-Identifier: GPL-3.0-or-later
//! 文档：画布尺寸、图层、撤销栈，以及把图层合成为可显示的像素。

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use crate::lang::tr;
use crate::selection::Selection;
use crate::stroke::{BrushParams, Stroke};
pub use crate::style::Style;
use crate::style::Effect;
use crate::trf;
use crate::tile::{tiles_in, GrayMap, Tile, TileCoord, TileMap, TILE};
use crate::{Rect, ONE};

mod structure;

const N: usize = (TILE * TILE) as usize;

/// 画布边长上限（规格 F-172）。
pub const MAX_SIDE: i32 = 16384;
/// 撤销栈最多保留的步数。
const MAX_UNDO_STEPS: usize = 300;
/// 撤销栈里旧分块占用的内存上限。超过后丢弃最旧的步骤，但至少保留 100 步（规格 F-181）。
const MAX_UNDO_BYTES: usize = 1 << 30;
const MIN_UNDO_STEPS: usize = 100;

/// 图层混合模式。公式取自 W3C《Compositing and Blending Level 1》。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    Add,
    Difference,
}

impl BlendMode {
    pub const ALL: [BlendMode; 8] = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::Add,
        BlendMode::Difference,
    ];

    /// 界面上显示的名称。
    pub fn label(self) -> &'static str {
        match self {
            BlendMode::Normal => tr("正常", "Normal"),
            BlendMode::Multiply => tr("正片叠底", "Multiply"),
            BlendMode::Screen => tr("滤色", "Screen"),
            BlendMode::Overlay => tr("叠加", "Overlay"),
            BlendMode::Darken => tr("变暗", "Darken"),
            BlendMode::Lighten => tr("变亮", "Lighten"),
            BlendMode::Add => tr("线性减淡", "Add"),
            BlendMode::Difference => tr("差值", "Difference"),
        }
    }

    /// 写进文件的标识。
    pub fn id(self) -> &'static str {
        match self {
            BlendMode::Normal => "normal",
            BlendMode::Multiply => "multiply",
            BlendMode::Screen => "screen",
            BlendMode::Overlay => "overlay",
            BlendMode::Darken => "darken",
            BlendMode::Lighten => "lighten",
            BlendMode::Add => "add",
            BlendMode::Difference => "difference",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.id() == id)
    }

    /// 底色 b 和源色 s（都是 0..=ONE 的非预乘通道值）混合后的颜色。
    #[inline]
    fn mix(self, b: u32, s: u32) -> u32 {
        match self {
            BlendMode::Normal => s,
            BlendMode::Multiply => mul15(b, s),
            BlendMode::Screen => b + s - mul15(b, s),
            BlendMode::Overlay => {
                if b * 2 <= ONE {
                    mul15(2 * b, s)
                } else {
                    ONE - mul15(2 * (ONE - b), ONE - s)
                }
            }
            BlendMode::Darken => b.min(s),
            BlendMode::Lighten => b.max(s),
            BlendMode::Add => (b + s).min(ONE),
            BlendMode::Difference => b.abs_diff(s),
        }
    }
}

/// 图层上除像素以外的属性。
#[derive(Clone, Debug, PartialEq)]
pub struct LayerProps {
    pub name: String,
    /// 0 到 1
    pub opacity: f32,
    pub visible: bool,
    pub blend: BlendMode,
    /// 保护不透明度：只能给已有的像素换颜色，透明的地方画不上，已有的也擦不掉（规格 F-053）
    pub lock_alpha: bool,
    /// 锁定：这一层的像素不能改，画、擦、填、清除、移动都不行
    pub locked: bool,
    /// 剪贴：只显示在下面那个图层有内容的地方（规格 F-054）
    pub clip: bool,
    /// 穿透（只对文件夹有意义）：里面的图层直接和文件夹下面的画面混合，不先合成一张再叠上去（实现方案 §3.11）
    pub pass_through: bool,
}

impl Default for LayerProps {
    fn default() -> Self {
        Self {
            name: String::new(),
            opacity: 1.0,
            visible: true,
            blend: BlendMode::Normal,
            lock_alpha: false,
            locked: false,
            clip: false,
            pass_through: false,
        }
    }
}

/// 图层的种类。
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// 普通的像素图层
    Pixels,
    /// 文件夹（规格 F-055）：紧挨在它下面、深度比它大的那一串都在它里面。`open` 是图层列表里展开着
    Folder { open: bool },
    /// 填充图层（规格 F-061）：不存像素，合成时按参数现算
    Fill(Fill),
}

/// 填充图层画的是什么。
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    /// 纯色
    Solid([u8; 3]),
    /// 渐变或图案：本程序还不会画（显示成透明），只原样保留，见 `Fill::psd`
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    pub paint: Paint,
    /// 从 PSD 读来、没有改过的原始描述符块（键和数据），存回 PSD 时原样写回去
    pub psd: Option<([u8; 4], Vec<u8>)>,
}

/// 图层蒙版（规格 F-058）：白处显示、黑处隐藏。
#[derive(Clone)]
pub struct Mask {
    pub tiles: GrayMap,
    /// 分块以外的地方的值：0（全黑）或 ONE（全白），只有这两种（文件里也只存得下这两种）
    pub default: u16,
    /// 停用：合成时不算它
    pub disabled: bool,
}

impl Mask {
    /// 全白（全部显示）的蒙版
    pub fn white() -> Self {
        Mask { tiles: GrayMap::default(), default: ONE as u16, disabled: false }
    }

    /// 分块 tc 上的蒙版值；没有分块时是 None，取默认值。
    pub fn tile(&self, tc: TileCoord) -> Option<&[u16]> {
        self.tiles.get(tc).map(|t| t.as_slice())
    }

    /// 默认值是不是偏白。偏白的蒙版，"墨"是它比白暗多少；偏黑的，是它比黑亮多少。
    fn white_default(&self) -> bool {
        self.default as u32 * 2 >= ONE
    }

    /// 把蒙版画成一张图：每个像素四个通道都是它离默认值多远（"墨"），分块以外就是透明。
    /// 这样蒙版就能走和像素一样的移动、变换（实现方案 §3.12），变完再用 `with_ink` 变回蒙版。
    pub fn to_ink(&self) -> TileMap {
        let white = self.white_default();
        let mut out = TileMap::default();
        for tc in self.tiles.coords() {
            let t = self.tiles.get(tc).unwrap();
            let mut px = vec![0u16; N * 4];
            for (k, &v) in t.iter().enumerate() {
                let ink = if white { ONE.saturating_sub(v as u32) } else { (v as u32).min(ONE) } as u16;
                px[k * 4..k * 4 + 4].fill(ink);
            }
            out.replace(tc, Some(Arc::new(Tile(px))));
        }
        out
    }

    /// `to_ink` 反过来：用一张"墨"图（只看透明度）做出默认值、停用与否都和这个蒙版一样的新蒙版。
    pub fn with_ink(&self, ink: &TileMap) -> Mask {
        let white = self.white_default();
        let mut tiles = GrayMap::default();
        for tc in ink.coords() {
            let t = ink.get(tc).unwrap();
            let v: Vec<u16> = (0..N).map(|k| t.0[k * 4 + 3] as u32).map(|a| if white { ONE - a.min(ONE) } else { a.min(ONE) } as u16).collect();
            tiles.replace(tc, Some(Arc::new(v)));
        }
        Mask { tiles, default: if white { ONE as u16 } else { 0 }, disabled: self.disabled }
    }
}

/// 一次换掉的一样东西：某一层的像素，或者它的蒙版（`replace_parts`）。
pub enum Part {
    Pixels(TileMap),
    Mask(Option<Mask>),
}

#[derive(Clone)]
pub struct Layer {
    pub props: LayerProps,
    /// 像素（只有像素图层有）
    pub tiles: TileMap,
    pub kind: Kind,
    /// 嵌套深度：0 是最外层，文件夹里面的比文件夹大 1
    pub depth: u16,
    pub mask: Option<Mask>,
    /// 图层样式（规格 F-062）
    pub style: Option<Style>,
    /// 样式画好的效果，按图层缓存
    pub fx: crate::style::Cache,
}

impl Layer {
    pub fn new(name: &str) -> Self {
        Self::pixels(LayerProps { name: name.into(), ..Default::default() }, TileMap::default())
    }

    /// 像素图层。
    pub fn pixels(props: LayerProps, tiles: TileMap) -> Self {
        Self { props, tiles, kind: Kind::Pixels, depth: 0, mask: None, style: None, fx: Default::default() }
    }

    /// 空的文件夹，新建的默认穿透。
    pub fn folder(name: &str) -> Self {
        Self {
            props: LayerProps { name: name.into(), pass_through: true, ..Default::default() },
            tiles: TileMap::default(),
            kind: Kind::Folder { open: true },
            depth: 0,
            mask: None,
            style: None,
            fx: Default::default(),
        }
    }

    pub fn is_folder(&self) -> bool {
        matches!(self.kind, Kind::Folder { .. })
    }

    pub fn is_pixels(&self) -> bool {
        self.kind == Kind::Pixels
    }
}

type TileSwap = Vec<(TileCoord, Option<Arc<Tile>>)>;

/// 撤销栈里的一个动作。每种动作保存的都是"另一个状态"，撤销和重做是同一个对调操作。
enum Action {
    /// 像素改动：保存被改动分块在另一个状态下的版本
    Paint { layer: usize, tiles: TileSwap },
    /// 整层像素被换掉（变换、填充）：保存另一个状态下的整张分块表。分块是共享的，没变的部分不多占内存
    Tiles { layer: usize, tiles: TileMap },
    /// 图层的有无：`layer` 为 Some 表示它现在不在文档里，对调时插回 `index`；为 None 则相反
    Presence { index: usize, layer: Option<Box<Layer>> },
    Props { index: usize, props: LayerProps },
    /// 图层的蒙版换掉（加、删、停用、在蒙版上画）：保存另一个状态下的蒙版
    Mask { index: usize, mask: Option<Mask> },
    /// 图层样式换掉（打开、关掉效果）：保存另一个状态下的样式
    Style { index: usize, style: Option<Style> },
    /// 整个图层换掉（应用蒙版、改填充颜色、栅格化、合并）：保存另一个状态下的这一层
    Swap { index: usize, layer: Box<Layer> },
    /// 重新排列（挪动、进出文件夹）：另一个状态下第 k 个位置上是现在的第 `order[k]` 层，深度是 `depths[k]`
    Arrange { order: Vec<usize>, depths: Vec<u16> },
    /// 几个动作合成一步，例如"向下合并"
    Group(Vec<Action>),
}

struct Step {
    id: u64,
    action: Action,
    /// 另一个状态下的当前图层
    active: usize,
    /// 这一步如果连选区也换了（移动、变换选中的内容），这里是另一个状态下的选区
    selection: Option<Option<Selection>>,
}

pub struct Document {
    pub width: i32,
    pub height: i32,
    /// 打印分辨率，像素每英寸。只是随文件保存的一个数字，不影响作画。
    pub dpi: u32,
    pub background: [u8; 3],
    layers: Vec<Layer>,
    active: usize,
    undo: Vec<Step>,
    redo: Vec<Step>,
    next_id: u64,
    /// 上次保存（或新建）时所处的状态；与当前状态相同就没有未保存的改动。
    saved_state: u64,
    layer_counter: usize,
    folder_counter: usize,
    /// 当前选区。有选区时，绘制、清空、填充只作用在选区里。
    selection: Option<Selection>,
    /// 编辑的是当前图层的蒙版而不是像素（规格 F-058）。当前图层没有蒙版时总是 false
    mask_target: bool,
}

impl Document {
    pub fn new(width: i32, height: i32) -> Self {
        Self::from_layers(width, height, [255, 255, 255], vec![Layer::new(tr("图层 1", "Layer 1"))])
    }

    /// 用现成的图层组成文档（读文件时用）。图层顺序自下而上，至少要有一层。
    pub fn from_layers(width: i32, height: i32, background: [u8; 3], mut layers: Vec<Layer>) -> Self {
        if layers.is_empty() {
            layers.push(Layer::new(tr("图层 1", "Layer 1")));
        }
        normalize_depths(&mut layers);
        let n = layers.len();
        let folders = layers.iter().filter(|l| l.is_folder()).count();
        Self {
            width: width.clamp(1, MAX_SIDE),
            height: height.clamp(1, MAX_SIDE),
            dpi: 300,
            background,
            active: n - 1,
            layer_counter: n - folders,
            folder_counter: folders,
            mask_target: false,
            layers,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            saved_state: 0,
            selection: None,
        }
    }

    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    /// 设置或取消选区。空选区等同于没有选区。选区的改动不进撤销栈。
    pub fn set_selection(&mut self, selection: Option<Selection>) {
        self.selection = selection.filter(|s| !s.is_empty());
    }

    pub fn bounds(&self) -> Rect {
        Rect { x0: 0, y0: 0, x1: self.width, y1: self.height }
    }

    /// 图层的像素最远能放到哪里：画布四周各再加一个画布的宽和高（实现方案 §3.15）。
    /// 读文件、移动、变换以它为界；合成、画笔、选区只看画布里。
    pub fn keep_bounds(&self) -> Rect {
        Self::keep_area(self.width, self.height)
    }

    /// `keep_bounds` 的算法，读文件时文档还没建起来就要用。
    pub fn keep_area(width: i32, height: i32) -> Rect {
        Rect { x0: -width, y0: -height, x1: 2 * width, y1: 2 * height }
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    pub fn active(&self) -> usize {
        self.active
    }

    /// 当前图层（像素或者蒙版）现在能不能画：看不见的、锁定的不能，在看不见或者锁定的文件夹里的也不能；
    /// 文件夹和填充图层本身没有像素可画，它们的蒙版可以画。界面据此决定要不要开始一笔。
    pub fn can_edit_active(&self) -> bool {
        let l = &self.layers[self.active];
        let target = if self.mask_target { l.mask.is_some() } else { l.is_pixels() };
        target && self.active_open()
    }

    /// 当前图层的像素现在能不能改：是像素图层、编辑的不是蒙版，而且能画（`can_edit_active`）。
    /// 油漆桶、液化、滤镜看这一条：它们对蒙版、文件夹和填充图层不能用（实现方案 §3.11、§3.12）。
    pub fn can_edit_pixels(&self) -> bool {
        !self.mask_target && self.layers[self.active].is_pixels() && self.active_open()
    }

    /// 第 i 层改不了：它自己或者外面哪一层组锁定着（实现方案 §3.11：组锁定就是里面的东西都锁定）。
    pub fn is_locked(&self, i: usize) -> bool {
        self.layers[i].props.locked || self.ancestors(i).into_iter().any(|j| self.layers[j].props.locked)
    }

    /// 当前图层和它外面的每一层文件夹都看得见、没锁（移动、变换只看这一条，文件夹也能移动）。
    pub fn active_open(&self) -> bool {
        let open = |i: usize| self.layers[i].props.visible && !self.layers[i].props.locked;
        open(self.active) && self.ancestors(self.active).into_iter().all(open)
    }

    pub fn set_active(&mut self, index: usize) {
        if index < self.layers.len() {
            self.active = index;
            self.mask_target = false;
        }
    }

    /// 编辑的是不是当前图层的蒙版。
    pub fn mask_target(&self) -> bool {
        self.mask_target
    }

    /// 在当前图层的像素和蒙版之间切换编辑对象。没有蒙版时总是像素。
    pub fn set_mask_target(&mut self, on: bool) {
        self.mask_target = on && self.layers[self.active].mask.is_some();
    }

    fn state(&self) -> u64 {
        self.undo.last().map_or(0, |s| s.id)
    }

    /// 是否有未保存的改动。撤销回保存点后为 false（规格 F-002）。
    pub fn is_modified(&self) -> bool {
        self.state() != self.saved_state
    }

    pub fn mark_saved(&mut self) {
        self.saved_state = self.state();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// 记下一步。`active_before` 是这一步之前的当前图层。
    fn push(&mut self, action: Action, active_before: usize) {
        let id = self.next_id;
        self.next_id += 1;
        self.undo.push(Step { id, action, active: active_before, selection: None });
        self.redo.clear();
        self.trim_undo();
    }

    fn trim_undo(&mut self) {
        fn bytes(a: &Action) -> usize {
            match a {
                Action::Paint { tiles, .. } => tiles.iter().filter(|t| t.1.is_some()).count() * N * 8,
                // 整层替换时两张表里大部分分块是共享的，这里按最坏情况估。
                Action::Tiles { tiles, .. } => tiles.len() * N * 8,
                Action::Presence { layer: Some(l), .. } | Action::Swap { layer: l, .. } => l.tiles.len() * N * 8 + l.mask.as_ref().map_or(0, |m| m.tiles.len() * N * 2),
                Action::Mask { mask: Some(m), .. } => m.tiles.len() * N * 2,
                Action::Group(v) => v.iter().map(bytes).sum(),
                _ => 0,
            }
        }
        while self.undo.len() > MAX_UNDO_STEPS {
            self.undo.remove(0);
        }
        let mut total: usize = self.undo.iter().map(|s| bytes(&s.action)).sum();
        while total > MAX_UNDO_BYTES && self.undo.len() > MIN_UNDO_STEPS {
            total -= bytes(&self.undo.remove(0).action);
        }
    }

    // ---- 像素操作 ----

    /// 抬笔后把一整笔并入当前图层，记为一个撤销步骤。返回被改动的画布区域。
    pub fn commit_stroke(&mut self, stroke: &Stroke) -> Rect {
        if self.mask_target {
            return self.commit_stroke_to_mask(stroke);
        }
        let bounds = self.bounds();
        let active = self.active;
        if self.is_locked(active) {
            return Rect::EMPTY;
        }
        let selection = self.selection.as_ref();
        let layer = &mut self.layers[active];
        if !layer.is_pixels() {
            return Rect::EMPTY;
        }
        let keep_alpha = layer.props.lock_alpha;
        let mut before = Vec::new();
        let mut dirty = Rect::EMPTY;
        for tc in stroke.mask.coords() {
            let tile_rect = tile_rect(tc);
            let clip = tile_rect.intersect(bounds);
            if clip.is_empty() {
                continue;
            }
            // 有选区时，选区在这个分块上没有内容就整块跳过。
            let selected = selection.map(|s| s.tile(tc));
            if selected == Some(None) {
                continue;
            }
            let mask = stroke.mask.get(tc).unwrap();
            before.push((tc, layer.tiles.get(tc).cloned()));
            let tile = layer.tiles.get_mut(tc);
            for y in clip.y0..clip.y1 {
                for x in clip.x0..clip.x1 {
                    let i = ((y - tile_rect.y0) * TILE + (x - tile_rect.x0)) as usize;
                    let cover = within(mask[i], selected.flatten(), i);
                    if cover != 0 {
                        let px: &mut [u16] = &mut tile.0[i * 4..i * 4 + 4];
                        let out = apply_stroke([px[0], px[1], px[2], px[3]], cover, &stroke.params, keep_alpha);
                        px.copy_from_slice(&out);
                    }
                }
            }
            dirty = dirty.union(clip);
        }
        if before.is_empty() {
            return Rect::EMPTY;
        }
        self.push(Action::Paint { layer: active, tiles: before }, active);
        dirty
    }

    /// 一笔画在当前图层的蒙版上（规格 F-058）：把蒙版往前景色的明度推，橡皮往白推。
    fn commit_stroke_to_mask(&mut self, stroke: &Stroke) -> Rect {
        let (bounds, active) = (self.bounds(), self.active);
        let Some(old) = self.layers[active].mask.clone() else { return Rect::EMPTY };
        if self.is_locked(active) {
            return Rect::EMPTY;
        }
        let mut mask = old.clone();
        let fill = mask.default;
        let mut dirty = Rect::EMPTY;
        for tc in stroke.mask.coords() {
            let (whole, clip) = (tile_rect(tc), tile_rect(tc).intersect(bounds));
            let selected = self.selection.as_ref().map(|s| s.tile(tc));
            if clip.is_empty() || selected == Some(None) {
                continue;
            }
            let cover = stroke.mask.get(tc).unwrap();
            let t = mask.tiles.get_mut(tc, fill);
            for y in clip.y0..clip.y1 {
                for x in clip.x0..clip.x1 {
                    let i = ((y - whole.y0) * TILE + (x - whole.x0)) as usize;
                    let c = within(cover[i], selected.flatten(), i);
                    if c != 0 {
                        t[i] = paint_mask(t[i], c, &stroke.params);
                    }
                }
            }
            dirty = dirty.union(clip);
        }
        if dirty.is_empty() {
            return Rect::EMPTY;
        }
        self.layers[active].mask = Some(mask);
        self.push(Action::Mask { index: active, mask: Some(old) }, active);
        dirty
    }

    /// 把当前图层的蒙版（有选区时只是选区里）改成 `value`（0..=ONE）。清除、填充在蒙版上就是这个。
    /// 没有选区、`value` 又不是全黑全白时，画布上的分块都填上它（默认值只能是全黑或全白）。
    fn rewrite_mask(&mut self, value: u16) -> Rect {
        let active = self.active;
        let Some(old) = self.layers[active].mask.clone() else { return Rect::EMPTY };
        if self.is_locked(active) {
            return Rect::EMPTY;
        }
        let new = match &self.selection {
            None if value == 0 || value as u32 == ONE => Mask { tiles: GrayMap::default(), default: value, disabled: old.disabled },
            None => {
                let mut m = Mask { tiles: GrayMap::default(), default: if value as u32 * 2 >= ONE { ONE as u16 } else { 0 }, disabled: old.disabled };
                for tc in tiles_in(self.bounds()) {
                    m.tiles.replace(tc, Some(Arc::new(vec![value; N])));
                }
                m
            }
            Some(sel) => {
                let mut m = old.clone();
                let fill = m.default;
                for tc in tiles_in(sel.bounds().intersect(self.bounds())) {
                    let Some(s) = sel.tile(tc).filter(|s| s.iter().any(|&v| v != 0)) else { continue };
                    let t = m.tiles.get_mut(tc, fill);
                    for (i, &v) in s.iter().enumerate() {
                        if v != 0 {
                            let v = (v as u32).min(ONE) as i64;
                            t[i] = (t[i] as i64 + ((value as i64 - t[i] as i64) * v + (ONE / 2) as i64).div_euclid(ONE as i64)) as u16;
                        }
                    }
                }
                m
            }
        };
        self.layers[active].mask = Some(new);
        self.push(Action::Mask { index: active, mask: Some(old) }, active);
        self.bounds()
    }

    /// 清空当前图层（规格 F-041）。编辑的是蒙版时，是把蒙版全部变白（全部显示）。
    pub fn clear_active(&mut self) -> Rect {
        if self.mask_target {
            return self.rewrite_mask(ONE as u16);
        }
        let active = self.active;
        if !self.layers[active].is_pixels() {
            return Rect::EMPTY;
        }
        // 清除是把像素变透明；保护不透明度的图层上这件事做不了。
        if self.is_locked(active) || self.layers[active].props.lock_alpha {
            return Rect::EMPTY;
        }
        if let Some(sel) = self.selection.as_ref() {
            // 有选区：只擦掉选区里的部分。
            let area = sel.bounds().intersect(self.bounds());
            let mut tiles = self.layers[active].tiles.clone();
            let mut changed = false;
            for tc in tiles_in(area) {
                let (Some(m), true) = (sel.tile(tc), tiles.get(tc).is_some()) else { continue };
                let t = tiles.get_mut(tc);
                for (i, &v) in m.iter().enumerate() {
                    if v != 0 {
                        for c in &mut t.0[i * 4..i * 4 + 4] {
                            *c = mul15(*c as u32, ONE - v as u32) as u16;
                        }
                        changed = true;
                    }
                }
            }
            return if changed { self.replace_layer_tiles(active, tiles) } else { Rect::EMPTY };
        }
        let coords: Vec<TileCoord> = tiles_in(self.bounds()).filter(|&tc| self.layers[active].tiles.get(tc).is_some()).collect();
        if coords.is_empty() {
            return Rect::EMPTY;
        }
        let layer = &mut self.layers[active];
        let before: TileSwap = coords.into_iter().map(|tc| (tc, layer.tiles.replace(tc, None))).collect();
        self.push(Action::Paint { layer: active, tiles: before }, active);
        self.bounds()
    }

    // 图层结构（新建、删除、挪动、文件夹、合并、蒙版）在子模块 `structure` 里。

    /// 用一个颜色填满当前图层；有选区时只填选区里（规格 F-057）。
    pub fn fill_active(&mut self, color: [u8; 3]) -> Rect {
        if self.mask_target {
            return self.rewrite_mask(luma(color) as u16);
        }
        let src = [to15(color[0]), to15(color[1]), to15(color[2]), ONE];
        self.rewrite_active(false, |_| src)
    }

    /// 把当前图层里的白色变成透明（规格 F-057），用来把扫描的白底线稿变成透明底。
    /// 处理后的图层叠在白底上，看起来和处理前一模一样。有选区时只处理选区里。
    pub fn white_to_alpha(&mut self) -> Rect {
        if self.layers[self.active].props.lock_alpha || self.mask_target {
            return Rect::EMPTY;
        }
        self.rewrite_active(true, |px| {
            // 先看它叠在白底上是什么颜色，再把其中"白"的成分整个抽掉。
            let over = [px[0] + ONE - px[3], px[1] + ONE - px[3], px[2] + ONE - px[3]];
            let white = over[0].min(over[1]).min(over[2]);
            [over[0] - white, over[1] - white, over[2] - white, ONE - white]
        })
    }

    /// 把当前图层的每个像素换成 `f` 算出来的值（预乘，15 位）；有选区时按选中的程度在原值和新值之间过渡。
    /// `existing_only` 为 true 时只处理已经有内容的分块。记为一个撤销步骤。
    fn rewrite_active(&mut self, existing_only: bool, f: impl Fn([u32; 4]) -> [u32; 4]) -> Rect {
        let bounds = self.bounds();
        let active = self.active;
        if self.is_locked(active) {
            return Rect::EMPTY;
        }
        if !self.layers[active].is_pixels() {
            return Rect::EMPTY;
        }
        let keep_alpha = self.layers[active].props.lock_alpha;
        let area = self.selection.as_ref().map_or(bounds, |s| s.bounds().intersect(bounds));
        let mut tiles = self.layers[active].tiles.clone();
        let mut changed = false;
        for tc in tiles_in(area) {
            let selected = self.selection.as_ref().map(|s| s.tile(tc));
            if selected == Some(None) || ((existing_only || keep_alpha) && tiles.get(tc).is_none()) {
                continue;
            }
            let (whole, clip) = (tile_rect(tc), tile_rect(tc).intersect(bounds));
            let t = tiles.get_mut(tc);
            for y in clip.y0..clip.y1 {
                for x in clip.x0..clip.x1 {
                    let i = ((y - whole.y0) * TILE + (x - whole.x0)) as usize;
                    let cover = within(ONE as u16, selected.flatten(), i) as u32;
                    if cover == 0 {
                        continue;
                    }
                    let px = [t.0[i * 4] as u32, t.0[i * 4 + 1] as u32, t.0[i * 4 + 2] as u32, t.0[i * 4 + 3] as u32];
                    let mut new = f(px);
                    if keep_alpha {
                        // 保护不透明度：只把颜色换过去，原来多透明还是多透明。
                        let na = new[3].max(1);
                        new = [new[0] * px[3] / na, new[1] * px[3] / na, new[2] * px[3] / na, px[3]];
                    }
                    let mixed = |c: usize| mul15(new[c], cover) + mul15(px[c], ONE - cover);
                    // 各通道分别四舍五入之后，颜色可能比透明度多出 1，压回去。
                    let alpha = mixed(3).min(ONE);
                    for c in 0..3 {
                        t.0[i * 4 + c] = mixed(c).min(alpha) as u16;
                    }
                    t.0[i * 4 + 3] = alpha as u16;
                    changed = true;
                }
            }
        }
        if changed {
            self.replace_layer_tiles(active, tiles)
        } else {
            Rect::EMPTY
        }
    }

    /// 把某一层的像素整个换掉，记为一个撤销步骤（变换、填充用）。返回需要重画的区域。
    pub fn replace_layer_tiles(&mut self, index: usize, tiles: TileMap) -> Rect {
        if self.is_locked(index) {
            return Rect::EMPTY;
        }
        let old = std::mem::replace(&mut self.layers[index].tiles, tiles);
        let active = self.active;
        self.push(Action::Tiles { layer: index, tiles: old }, active);
        self.bounds()
    }

    /// 同 `replace_layer_tiles`，同时把选区换成 `selection`。撤销时选区跟着像素一起回去，
    /// 不然移动了选中的内容再撤销，选框会留在新位置上。
    pub fn replace_layer_tiles_and_selection(&mut self, index: usize, tiles: TileMap, selection: Option<Selection>) -> Rect {
        if self.is_locked(index) {
            return Rect::EMPTY;
        }
        let before = std::mem::replace(&mut self.selection, selection.filter(|s| !s.is_empty()));
        let dirty = self.replace_layer_tiles(index, tiles);
        if let Some(step) = self.undo.last_mut() {
            step.selection = Some(before);
        }
        dirty
    }

    /// 把几层的像素、蒙版一起换掉，记成一个撤销步骤（文件夹整个移动、变换，蒙版跟着图层变）。
    /// `selection` 是 Some 时选区也换掉，撤销时一起回去。锁定的图层不换。返回需要重画的区域。
    pub fn replace_parts(&mut self, parts: Vec<(usize, Part)>, selection: Option<Option<Selection>>) -> Rect {
        let mut actions = Vec::new();
        for (index, part) in parts {
            if self.is_locked(index) {
                continue;
            }
            actions.push(match part {
                Part::Pixels(tiles) => Action::Tiles { layer: index, tiles: std::mem::replace(&mut self.layers[index].tiles, tiles) },
                Part::Mask(mask) => Action::Mask { index, mask: std::mem::replace(&mut self.layers[index].mask, mask) },
            });
        }
        if actions.is_empty() {
            return Rect::EMPTY;
        }
        let active = self.active;
        self.push(Action::Group(actions), active);
        if let Some(sel) = selection {
            let before = std::mem::replace(&mut self.selection, sel.filter(|s| !s.is_empty()));
            if let Some(step) = self.undo.last_mut() {
                step.selection = Some(before);
            }
        }
        self.bounds()
    }

    /// 把某一层的蒙版换掉但不记撤销，用于变换时的实时预览。返回换下来的旧蒙版。
    pub fn set_mask_live(&mut self, index: usize, mask: Option<Mask>) -> Option<Mask> {
        std::mem::replace(&mut self.layers[index].mask, mask)
    }

    /// 把某一层的像素整个换掉但不记撤销，用于变换时的实时预览。返回换下来的旧像素。
    pub fn set_layer_tiles_live(&mut self, index: usize, tiles: TileMap) -> TileMap {
        std::mem::replace(&mut self.layers[index].tiles, tiles)
    }

    /// 只换掉某一层的几个分块，不记撤销（液化时的实时预览，每动一下只换笔刷碰到的那几块）。
    pub fn set_layer_part_live(&mut self, index: usize, tiles: impl IntoIterator<Item = (TileCoord, Option<Arc<Tile>>)>) {
        let layer = &mut self.layers[index].tiles;
        for (tc, t) in tiles {
            layer.replace(tc, t);
        }
    }

    /// 改图层属性但不记撤销，用于拖动滑块时的实时预览。
    pub fn set_props_live(&mut self, index: usize, props: LayerProps) {
        if let Some(l) = self.layers.get_mut(index) {
            l.props = props;
        }
    }

    /// 一次属性调整结束：如果与调整前不同，记为一个撤销步骤。
    pub fn commit_props(&mut self, index: usize, before: LayerProps) {
        if self.layers.get(index).is_some_and(|l| l.props != before) {
            let active = self.active;
            self.push(Action::Props { index, props: before }, active);
        }
    }

    // ---- 撤销 ----

    fn apply(layers: &mut Vec<Layer>, action: &mut Action, undoing: bool) -> Rect {
        let everything = Rect { x0: i32::MIN / 2, y0: i32::MIN / 2, x1: i32::MAX / 2, y1: i32::MAX / 2 };
        match action {
            Action::Paint { layer, tiles } => {
                let l = &mut layers[*layer];
                let mut dirty = Rect::EMPTY;
                for (tc, slot) in tiles.iter_mut() {
                    *slot = l.tiles.replace(*tc, slot.take());
                    dirty = dirty.union(tile_rect(*tc));
                }
                dirty
            }
            Action::Tiles { layer, tiles } => {
                std::mem::swap(&mut layers[*layer].tiles, tiles);
                everything
            }
            Action::Presence { index, layer } => {
                match layer.take() {
                    Some(l) => layers.insert(*index, *l),
                    None => *layer = Some(Box::new(layers.remove(*index))),
                }
                everything
            }
            Action::Props { index, props } => {
                std::mem::swap(&mut layers[*index].props, props);
                everything
            }
            Action::Swap { index, layer } => {
                std::mem::swap(&mut layers[*index], layer);
                everything
            }
            Action::Mask { index, mask } => {
                std::mem::swap(&mut layers[*index].mask, mask);
                everything
            }
            Action::Style { index, style } => {
                std::mem::swap(&mut layers[*index].style, style);
                everything
            }
            Action::Arrange { order, depths } => {
                // 换成另一个状态，同时记下换回来要用的顺序和深度。
                let old: Vec<u16> = layers.iter().map(|l| l.depth).collect();
                let mut taken: Vec<Option<Layer>> = std::mem::take(layers).into_iter().map(Some).collect();
                let mut back = vec![0; order.len()];
                for (k, &from) in order.iter().enumerate() {
                    let mut l = taken[from].take().expect("排列里每一层恰好出现一次");
                    l.depth = depths[k];
                    layers.push(l);
                    back[from] = k;
                }
                // 换回去时第 j 个位置上是现在的第 back[j] 层，深度是它原来在第 j 个位置时的深度。
                *depths = old;
                *order = back;
                everything
            }
            Action::Group(actions) => {
                let mut dirty = Rect::EMPTY;
                if undoing {
                    for a in actions.iter_mut().rev() {
                        dirty = dirty.union(Self::apply(layers, a, undoing));
                    }
                } else {
                    for a in actions.iter_mut() {
                        dirty = dirty.union(Self::apply(layers, a, undoing));
                    }
                }
                dirty
            }
        }
    }

    /// 撤销、重做之后：当前图层换了，编辑对象回到像素；当前图层没有蒙版了，也回到像素。
    fn settle_mask_target(&mut self, before: usize) {
        self.mask_target = self.mask_target && self.active == before && self.layers[self.active].mask.is_some();
    }

    /// 撤销一步，返回需要重画的画布区域。
    pub fn undo(&mut self) -> Rect {
        let Some(mut step) = self.undo.pop() else { return Rect::EMPTY };
        let dirty = Self::apply(&mut self.layers, &mut step.action, true);
        std::mem::swap(&mut self.active, &mut step.active);
        self.settle_mask_target(step.active);
        if let Some(other) = step.selection.as_mut() {
            std::mem::swap(&mut self.selection, other);
        }
        self.redo.push(step);
        dirty.intersect(self.bounds())
    }

    pub fn redo(&mut self) -> Rect {
        let Some(mut step) = self.redo.pop() else { return Rect::EMPTY };
        let dirty = Self::apply(&mut self.layers, &mut step.action, false);
        std::mem::swap(&mut self.active, &mut step.active);
        self.settle_mask_target(step.active);
        if let Some(other) = step.selection.as_mut() {
            std::mem::swap(&mut self.selection, other);
        }
        self.undo.push(step);
        dirty.intersect(self.bounds())
    }

    // ---- 合成 ----

    /// 把一个分块位置上的图层自下而上叠到 `acc`（预乘，15 位）上。
    /// `pick` 说哪些图层算数：显示时是看得见的那些，合并时是要合并的那些；文件夹不算数时里面的都不算。
    /// `stroke` 是正在画、尚未并入的那一笔，叠在当前图层（或它的蒙版）上。
    fn accumulate(&self, tc: TileCoord, acc: &mut [[u32; 4]], pick: impl Fn(usize) -> bool, stroke: Option<&Stroke>) {
        let live = Live::new(self, tc, stroke);
        self.level(tc, acc, 0..self.layers.len(), &pick, &live);
    }

    /// 把 `range` 里最外层的兄弟（连同文件夹里面的东西）自下而上叠到 `acc` 上（实现方案 §3.11）。
    /// 一个文件夹里面的东西，就是排在它前面、上一个兄弟后面的那一串。
    fn level(&self, tc: TileCoord, acc: &mut [[u32; 4]], range: std::ops::Range<usize>, pick: &dyn Fn(usize) -> bool, live: &Live) {
        let Some(depth) = self.layers[range.clone()].iter().map(|l| l.depth).min() else { return };
        let siblings: Vec<usize> = range.clone().filter(|&i| self.layers[i].depth == depth).collect();
        // 剪贴图层的遮罩：它下面最近的那个不剪贴的兄弟（它的"底"）在这个分块上各像素的不透明度。
        // 空着表示底在这里没有内容，剪贴在它上面的图层也就什么都不显示。
        let mut base_alpha: Vec<u16> = Vec::new();
        let mut base_shown = false;
        let mut from = range.start;
        for (k, &li) in siblings.iter().enumerate() {
            let children = from..li;
            from = li + 1;
            let layer = &self.layers[li];
            let clipped = layer.props.clip && k > 0;
            if !clipped {
                base_shown = pick(li);
                base_alpha.clear();
            }
            if !pick(li) || (clipped && (!base_shown || base_alpha.is_empty())) {
                continue;
            }
            // 上面紧跟着剪贴的兄弟的话，这一层各像素的不透明度要留下来给它们当遮罩。
            let feeds = !clipped && siblings.get(k + 1).is_some_and(|&j| self.layers[j].props.clip);
            // 里面的东西在这个分块上都没有内容（也没在画）的组什么也不添，整个跳过；拿它当底的剪贴图层也就什么都不显示。
            if layer.is_folder()
                && !children.clone().any(|i| {
                    let l = &self.layers[i];
                    l.tiles.get(tc).is_some() || matches!(l.kind, Kind::Fill(_)) || live.touches(i)
                })
            {
                continue;
            }
            let opacity = (layer.props.opacity.clamp(0.0, 1.0) * ONE as f32) as u32;
            let mask = self.mask_values(li, tc, live);

            if layer.is_folder() && layer.props.pass_through {
                // 穿透：里面的东西直接叠在一份拷贝上，再按不透明度（乘蒙版、剪贴）在原来和拷贝之间过渡。
                let mut copy = acc.to_vec();
                self.level(tc, &mut copy, children.clone(), pick, live);
                if feeds {
                    // 当底用的是它里面合出来的透明度，这要另外按隔离的方式合一遍。
                    let mut alone = vec![[0u32; 4]; N];
                    self.level(tc, &mut alone, children, pick, live);
                    base_alpha = (0..N).map(|i| within_mask(alone[i][3] as u16, mask.as_deref(), i)).collect();
                }
                for (i, (dst, c)) in acc.iter_mut().zip(&copy).enumerate() {
                    let mut f = within_mask(opacity as u16, mask.as_deref(), i) as u32;
                    if clipped {
                        f = mul15(f, base_alpha[i] as u32);
                    }
                    if f != 0 {
                        *dst = lerp4(*dst, *c, f);
                    }
                }
                continue;
            }

            // 这一层自己的像素（预乘）：像素图层取分块，纯色填充现算，隔离的文件夹先把里面的合出来。
            // 还不会画的填充图层显示文件里存着的像素（没有就是透明的）。
            let own: Option<Vec<[u16; 4]>> = match &layer.kind {
                Kind::Pixels | Kind::Fill(Fill { paint: Paint::Other, .. }) => None,
                Kind::Fill(fill) => Some(fill_pixels(fill)),
                Kind::Folder { .. } => {
                    let mut sub = vec![[0u32; 4]; N];
                    self.level(tc, &mut sub, children, pick, live);
                    Some(sub.iter().map(|p| p.map(|v| v.min(ONE) as u16)).collect())
                }
            };
            let tile = layer.tiles.get(tc).filter(|_| own.is_none());
            let painting = live.on_pixels(li);

            // 带样式的图层（实现方案 §3.14）：投影、外发光叠在下面的东西上，再叠图层本身（它自己的混合模式），
            // 再叠内发光；最后整体和原来的样子按图层的不透明度（剪贴时再乘底的透明度）过渡。
            if let Some(style) = layer.style.as_ref().filter(|s| s.shows() && own.is_none()) {
                let planes = layer.fx.get(&layer.tiles, layer.mask.as_ref(), &style.effects, self.bounds());
                let fx: Vec<(&Effect, &[u16])> =
                    style.effects.iter().zip(&planes.fx).filter_map(|(e, p)| p.get(&tc).map(|t| (e, t.as_slice()))).collect();
                if feeds {
                    base_alpha.resize(N, 0);
                }
                if tile.is_none() && !painting && fx.is_empty() {
                    continue;
                }
                let paint = |e: &Effect, v: u16| -> [u16; 4] {
                    let a = mul15(v as u32, (e.opacity.clamp(0.0, 1.0) * ONE as f32) as u32);
                    [mul15(to15(e.color[0]), a) as u16, mul15(to15(e.color[1]), a) as u16, mul15(to15(e.color[2]), a) as u16, a as u16]
                };
                for (i, dst) in acc.iter_mut().enumerate() {
                    let mut out = *dst;
                    for (e, t) in fx.iter().filter(|(e, _)| e.below()) {
                        if t[i] != 0 {
                            out = blend(out, paint(e, t[i]), ONE, e.blend);
                        }
                    }
                    let mut px = tile.map_or([0; 4], |t| [t.0[i * 4], t.0[i * 4 + 1], t.0[i * 4 + 2], t.0[i * 4 + 3]]);
                    if painting {
                        let m = live.cover(i);
                        if m != 0 {
                            px = apply_stroke(px, m, live.params(), layer.props.lock_alpha);
                        }
                    }
                    if let Some(m) = mask.as_deref() {
                        let v = m[i] as u32;
                        px = px.map(|c| mul15(c as u32, v) as u16);
                    }
                    if feeds {
                        base_alpha[i] = px[3];
                    }
                    if px[3] != 0 {
                        out = blend(out, px, ONE, layer.props.blend);
                    }
                    for (e, t) in fx.iter().filter(|(e, _)| !e.below()) {
                        if t[i] != 0 {
                            out = blend(out, paint(e, t[i]), ONE, e.blend);
                        }
                    }
                    let f = if clipped { mul15(opacity, base_alpha[i] as u32) } else { opacity };
                    if f != 0 {
                        *dst = lerp4(*dst, out, f);
                    }
                }
                continue;
            }

            if own.is_none() && tile.is_none() && !painting {
                if feeds {
                    base_alpha.resize(N, 0);
                }
                continue;
            }
            if feeds {
                base_alpha.resize(N, 0);
            }
            // 最常见的情况（普通图层，没有蒙版、剪贴，也不在画它）单走一条不带分支的路。
            if let (None, Some(t), false, None, false, false) = (&own, tile, painting, &mask, feeds, clipped) {
                for (dst, p) in acc.iter_mut().zip(t.0.as_chunks::<4>().0) {
                    if p[3] != 0 {
                        *dst = blend(*dst, *p, opacity, layer.props.blend);
                    }
                }
                continue;
            }
            for (i, dst) in acc.iter_mut().enumerate() {
                let mut px = match (&own, tile) {
                    (Some(o), _) => o[i],
                    (None, Some(t)) => [t.0[i * 4], t.0[i * 4 + 1], t.0[i * 4 + 2], t.0[i * 4 + 3]],
                    (None, None) => [0; 4],
                };
                if painting {
                    let m = live.cover(i);
                    if m != 0 {
                        px = apply_stroke(px, m, live.params(), layer.props.lock_alpha);
                    }
                }
                if let Some(m) = mask.as_deref() {
                    let v = m[i] as u32;
                    px = px.map(|c| mul15(c as u32, v) as u16);
                }
                if feeds {
                    base_alpha[i] = px[3];
                }
                if clipped {
                    let a = base_alpha[i] as u32;
                    px = px.map(|v| mul15(v as u32, a) as u16);
                }
                if px[3] != 0 {
                    *dst = blend(*dst, px, opacity, layer.props.blend);
                }
            }
        }
    }

    /// 第 li 层的蒙版在分块 tc 上的值（停用的、没有的是 None）；正在画蒙版时连那一笔一起算进去。
    fn mask_values(&self, li: usize, tc: TileCoord, live: &Live) -> Option<Vec<u16>> {
        let mask = self.layers[li].mask.as_ref().filter(|m| !m.disabled)?;
        let mut v = match mask.tile(tc) {
            Some(t) => t.to_vec(),
            None => vec![mask.default; N],
        };
        if live.on_mask(li) {
            for (i, m) in v.iter_mut().enumerate() {
                let c = live.cover(i);
                if c != 0 {
                    *m = paint_mask(*m, c, live.params());
                }
            }
        }
        Some(v)
    }

    /// 把一个分块位置上的全部可见图层合成为不透明的显示像素（0x00RRGGBB）。
    /// `stroke` 是正在画、尚未并入图层的那一笔，叠在当前图层上显示。
    pub fn composite_tile(&self, stroke: Option<&Stroke>, tc: TileCoord, out: &mut [u32]) {
        debug_assert_eq!(out.len(), N);
        let bg = [to15(self.background[0]), to15(self.background[1]), to15(self.background[2]), ONE];
        let mut acc = vec![bg; N];
        self.accumulate(tc, &mut acc, |i| self.layers[i].props.visible, stroke);
        for (o, a) in out.iter_mut().zip(&acc) {
            *o = (to8(a[0]) << 16) | (to8(a[1]) << 8) | to8(a[2]);
        }
    }

    /// 画布上某一点合成后的颜色（吸管用，规格 F-092）。画布外返回 None。
    pub fn pick(&self, x: i32, y: i32) -> Option<[u8; 3]> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        let mut out = vec![0u32; N];
        self.composite_tile(None, (x.div_euclid(TILE), y.div_euclid(TILE)), &mut out);
        let v = out[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize];
        Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
    }

    /// 整张画布合成后的 RGB 像素（每通道 8 位，逐行）。
    pub fn flatten_rgb(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgb = vec![0u8; w * h * 3];
        let mut tile = vec![0u32; N];
        for tc in tiles_in(self.bounds()) {
            self.composite_tile(None, tc, &mut tile);
            let r = tile_rect(tc).intersect(self.bounds());
            for y in r.y0..r.y1 {
                for x in r.x0..r.x1 {
                    let v = tile[((y - tc.1 * TILE) * TILE + (x - tc.0 * TILE)) as usize];
                    let o = (y as usize * w + x as usize) * 3;
                    rgb[o] = (v >> 16) as u8;
                    rgb[o + 1] = (v >> 8) as u8;
                    rgb[o + 2] = v as u8;
                }
            }
        }
        rgb
    }

    /// 把整张画布合成后导出为 PNG（不透明，RGB 各 8 位）。
    pub fn export_png(&self, path: &Path) -> std::io::Result<()> {
        let rgb = self.flatten_rgb();
        let mut enc = png::Encoder::new(BufWriter::new(File::create(path)?), self.width as u32, self.height as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(std::io::Error::other)?;
        wr.write_image_data(&rgb).map_err(std::io::Error::other)
    }
}

fn tile_rect(tc: TileCoord) -> Rect {
    Rect { x0: tc.0 * TILE, y0: tc.1 * TILE, x1: (tc.0 + 1) * TILE, y1: (tc.1 + 1) * TILE }
}

#[inline]
fn to15(v: u8) -> u32 {
    (v as u32 * ONE + 127) / 255
}

#[inline]
fn to8(v: u32) -> u32 {
    (v.min(ONE) * 255 + ONE / 2) >> 15
}

#[inline]
fn mul15(a: u32, b: u32) -> u32 {
    (a * b + ONE / 2) >> 15
}

/// 一笔的覆盖率受选区限制后的值。`selected` 是选区在这个分块上的内容，None 表示没有选区。
#[inline]
fn within(cover: u16, selected: Option<&[u16]>, i: usize) -> u16 {
    match selected {
        Some(s) => mul15(cover as u32, s[i] as u32) as u16,
        None => cover,
    }
}

/// 正在画、尚未并入的那一笔在一个分块上的样子：画在哪一层（像素还是蒙版）、各像素的覆盖率。
struct Live<'a> {
    stroke: Option<&'a Stroke>,
    active: usize,
    on_mask: bool,
    mask: Option<&'a [u16]>,
    tail: Option<&'a [u16]>,
    /// 选区在这个分块上的内容；外层 None 是没有选区，内层 None 是选区在这里没有内容
    selected: Option<Option<&'a [u16]>>,
}

impl<'a> Live<'a> {
    fn new(doc: &'a Document, tc: TileCoord, stroke: Option<&'a Stroke>) -> Self {
        let (mask, tail) = stroke.map_or((None, None), |s| s.masks(tc));
        Live { stroke, active: doc.active, on_mask: doc.mask_target, mask, tail, selected: doc.selection.as_ref().map(|s| s.tile(tc)) }
    }

    fn touches(&self, li: usize) -> bool {
        self.stroke.is_some() && li == self.active && (self.mask.is_some() || self.tail.is_some()) && self.selected != Some(None)
    }

    fn on_pixels(&self, li: usize) -> bool {
        !self.on_mask && self.touches(li)
    }

    fn on_mask(&self, li: usize) -> bool {
        self.on_mask && self.touches(li)
    }

    /// 第 i 个像素的覆盖率，已经受选区限制。
    #[inline]
    fn cover(&self, i: usize) -> u16 {
        within(self.mask.map_or(0, |m| m[i]).max(self.tail.map_or(0, |m| m[i])), self.selected.flatten(), i)
    }

    fn params(&self) -> &BrushParams {
        &self.stroke.expect("只在有笔画时调用").params
    }
}

/// 填充图层在一个分块上的像素（预乘）。纯色铺满；还不会画的种类是透明的。
fn fill_pixels(fill: &Fill) -> Vec<[u16; 4]> {
    match fill.paint {
        Paint::Solid(c) => vec![[to15(c[0]) as u16, to15(c[1]) as u16, to15(c[2]) as u16, ONE as u16]; N],
        Paint::Other => vec![[0; 4]; N],
    }
}

/// v 乘上蒙版在第 i 个像素的值；没有蒙版时不变。
#[inline]
fn within_mask(v: u16, mask: Option<&[u16]>, i: usize) -> u16 {
    mask.map_or(v, |m| mul15(v as u32, m[i] as u32) as u16)
}

/// 两个预乘像素之间按 f（0..=ONE）过渡。两头都是合法的预乘像素，结果也是。
#[inline]
fn lerp4(a: [u32; 4], b: [u32; 4], f: u32) -> [u32; 4] {
    let mix = |x: u32, y: u32| ((x as i64 + ((y as i64 - x as i64) * f as i64 + (ONE / 2) as i64).div_euclid(ONE as i64)) as u32).min(ONE);
    let alpha = mix(a[3], b[3]);
    [mix(a[0], b[0]).min(alpha), mix(a[1], b[1]).min(alpha), mix(a[2], b[2]).min(alpha), alpha]
}

/// 笔落在蒙版上（实现方案 §3.12）：把蒙版往"前景色的明度"推（橡皮往白推），强度是覆盖率乘浓度。
#[inline]
fn paint_mask(m: u16, cover: u16, params: &BrushParams) -> u16 {
    let a = mul15(cover as u32, (params.density.clamp(0.0, 1.0) * ONE as f32) as u32) as i64;
    let target = if params.erase { ONE } else { luma(params.color) } as i64;
    (m as i64 + ((target - m as i64) * a + (ONE / 2) as i64).div_euclid(ONE as i64)).clamp(0, ONE as i64) as u16
}

/// 颜色的明度（0..=ONE）：0.299 R + 0.587 G + 0.114 B。
pub fn luma(c: [u8; 3]) -> u32 {
    (to15(c[0]) * 299 + to15(c[1]) * 587 + to15(c[2]) * 114 + 500) / 1000
}

/// 让深度满足文件夹的规矩（实现方案 §3.11）：从上往下看，最上面一行是 0，下一行最多比这一行深 1，
/// 而且只有这一行是文件夹时才能更深。坏文件里不满足的地方往浅里收。
fn normalize_depths(layers: &mut [Layer]) {
    let mut allowed = 0u16;
    for l in layers.iter_mut().rev() {
        l.depth = l.depth.min(allowed);
        allowed = l.depth + l.is_folder() as u16;
    }
}

/// 把源像素（预乘）按不透明度和混合模式叠到底像素（预乘）上。
///
/// 结果 = 源独占的部分 + 两者重叠处的混合色 + 底独占的部分，
/// 即 W3C 合成规范里"先混合再做 source-over"的展开式。
#[inline]
fn blend(dst: [u32; 4], src: [u16; 4], opacity: u32, mode: BlendMode) -> [u32; 4] {
    let sa = mul15(src[3] as u32, opacity);
    if sa == 0 {
        return dst;
    }
    let da = dst[3];
    let mut out = [0u32; 4];
    out[3] = (sa + mul15(da, ONE - sa)).min(ONE);
    let alpha = out[3];
    for (c, o) in out.iter_mut().take(3).enumerate() {
        let sc = mul15(src[c] as u32, opacity); // 预乘后的源色
        let v = if mode == BlendMode::Normal || da == 0 {
            sc + mul15(dst[c], ONE - sa)
        } else {
            // 混合公式要用非预乘的颜色。
            let s = (src[c] as u32 * ONE / src[3] as u32).min(ONE);
            let b = (dst[c] * ONE / da).min(ONE);
            let both = mul15(sa, da);
            mul15(sc, ONE - da) + mul15(both, mode.mix(b, s)) + mul15(dst[c], ONE - sa)
        };
        // 预乘像素的颜色不能超过它的透明度。几项各自四舍五入后加起来可能多出 1，这里压回去；
        // 否则这种像素存不进"非预乘"的文件格式，存盘再读回会差一点。
        *o = v.min(alpha);
    }
    out
}

/// 马克笔落在一个像素上（规格 F-031）。`a` 是这一笔在这里的强度（覆盖率乘浓度）。
///
/// 不透明度取原有的和这一笔的较大者，所以怎么叠都不会越叠越浓；保护不透明度时保持原样。
/// 颜色取普通覆盖时会得到的那个颜色：空白处就是笔色，同色叠涂不变，换色时按这一笔的强度向新色过渡。
fn marker(px: [u16; 4], a: u32, color: [u8; 3], keep_alpha: bool) -> [u16; 4] {
    let pa = px[3] as u32;
    let keep = ONE - a;
    // 普通覆盖时会得到的不透明度；为零说明这里原来是空的、这一笔也没落下来。
    let over = a + mul15(pa, keep);
    if over == 0 {
        return px;
    }
    let alpha = if keep_alpha { pa } else { a.max(pa) };
    let mix = |c: usize| {
        // 普通覆盖时的预乘色，换成直色再乘上新的不透明度。
        let co = mul15(to15(color[c]), a) + mul15(px[c] as u32, keep);
        ((co as u64 * alpha as u64 + over as u64 / 2) / over as u64).min(alpha as u64) as u16
    };
    [mix(0), mix(1), mix(2), alpha as u16]
}

/// 把一笔在某个像素上的覆盖率作用到图层像素上：上色，或者擦除。
/// `keep_alpha` 是"保护不透明度"：只换颜色，不动透明度，橡皮不起作用。
fn apply_stroke(px: [u16; 4], coverage: u16, params: &BrushParams, keep_alpha: bool) -> [u16; 4] {
    let a = mul15(coverage as u32, (params.density.clamp(0.0, 1.0) * ONE as f32) as u32);
    let keep = ONE - a;
    if params.marker && !params.erase {
        return marker(px, a, params.color, keep_alpha);
    }
    if keep_alpha {
        if params.erase || px[3] == 0 {
            return px;
        }
        let pa = px[3] as u32;
        let mix = |c: usize| (mul15(mul15(to15(params.color[c]), pa), a) + mul15(px[c] as u32, keep)).min(pa) as u16;
        return [mix(0), mix(1), mix(2), px[3]];
    }
    if params.erase {
        return [
            mul15(px[0] as u32, keep) as u16,
            mul15(px[1] as u32, keep) as u16,
            mul15(px[2] as u32, keep) as u16,
            mul15(px[3] as u32, keep) as u16,
        ];
    }
    let src = [to15(params.color[0]), to15(params.color[1]), to15(params.color[2])];
    [
        (mul15(src[0], a) + mul15(px[0] as u32, keep)) as u16,
        (mul15(src[1], a) + mul15(px[1] as u32, keep)) as u16,
        (mul15(src[2], a) + mul15(px[2] as u32, keep)) as u16,
        (a + mul15(px[3] as u32, keep)).min(ONE) as u16,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stabilizer::Sample;

    fn draw(doc: &mut Document, y: f32, color: [u8; 3]) {
        let params = BrushParams { color, ..Default::default() };
        let (mut s, _) = Stroke::begin(params, 0, Sample { x: 10.0, y, p: 1.0 });
        s.add(Sample { x: 100.0, y, p: 1.0 });
        s.end();
        doc.commit_stroke(&s);
    }

    fn draw_line(doc: &mut Document, y: f32) {
        draw(doc, y, [0, 0, 0]);
    }

    fn pixel(doc: &Document, x: i32, y: i32) -> u32 {
        let c = doc.pick(x, y).unwrap();
        ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32
    }

    const WHITE: u32 = 0x00FF_FFFF;

    #[test]
    fn blank_canvas_is_background_and_a_stroke_is_black() {
        let mut doc = Document::new(200, 100);
        assert_eq!(pixel(&doc, 50, 30), WHITE);
        draw_line(&mut doc, 30.5);
        assert_eq!(pixel(&doc, 50, 30), 0);
        assert_eq!(pixel(&doc, 50, 60), WHITE);
        assert_eq!(doc.pick(-1, 0), None);
    }

    /// 规格 F-180 / F-002：一笔一步；撤销回初始状态后没有未保存的改动。
    #[test]
    fn undo_is_per_stroke_and_restores_the_clean_state() {
        let mut doc = Document::new(200, 100);
        assert!(!doc.is_modified());
        for k in 0..3 {
            draw_line(&mut doc, 20.5 + 20.0 * k as f32);
        }
        assert!(doc.is_modified());
        doc.undo();
        assert_eq!(pixel(&doc, 50, 60), WHITE);
        assert_eq!(pixel(&doc, 50, 40), 0);
        doc.undo();
        doc.undo();
        assert_eq!(pixel(&doc, 50, 20), WHITE);
        assert!(!doc.is_modified());
        assert!(!doc.can_undo());
        doc.redo();
        assert_eq!(pixel(&doc, 50, 20), 0);
        assert!(doc.is_modified());
    }

    /// 规格 M1 验收：3 笔加 1 层，撤销 4 次回到初始。
    #[test]
    fn three_strokes_and_a_layer_undo_in_four_steps() {
        let mut doc = Document::new(200, 100);
        draw_line(&mut doc, 20.5);
        draw_line(&mut doc, 40.5);
        doc.add_layer();
        draw_line(&mut doc, 60.5);
        assert_eq!((doc.layers().len(), doc.active()), (2, 1));
        for _ in 0..4 {
            doc.undo();
        }
        assert_eq!((doc.layers().len(), doc.active()), (1, 0));
        assert_eq!(pixel(&doc, 50, 20), WHITE);
        assert!(!doc.is_modified() && !doc.can_undo());
        for _ in 0..4 {
            doc.redo();
        }
        assert_eq!((doc.layers().len(), doc.active()), (2, 1));
        assert_eq!(pixel(&doc, 50, 60), 0);
    }

    /// 规格 F-181：连画 100 笔后可全部撤销。
    #[test]
    fn a_hundred_strokes_can_all_be_undone() {
        let mut doc = Document::new(400, 400);
        for k in 0..100 {
            draw_line(&mut doc, 3.5 + 3.0 * k as f32);
        }
        assert_eq!(doc.undo_depth(), 100);
        for _ in 0..100 {
            doc.undo();
        }
        assert!(!doc.is_modified());
        assert_eq!(pixel(&doc, 50, 3), WHITE);
    }

    #[test]
    fn layer_order_visibility_and_opacity_affect_the_result() {
        let mut doc = Document::new(200, 100);
        draw(&mut doc, 30.5, [255, 0, 0]);
        doc.add_layer();
        draw(&mut doc, 30.5, [0, 0, 255]);
        assert_eq!(pixel(&doc, 50, 30), 0x0000_00FF);
        // 把上层挪到下面，红色盖住蓝色。
        assert!(doc.move_active(-1));
        assert_eq!(doc.active(), 0);
        assert_eq!(pixel(&doc, 50, 30), 0x00FF_0000);
        doc.undo();
        assert_eq!(pixel(&doc, 50, 30), 0x0000_00FF);
        // 上层半透明。
        let before = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { opacity: 0.5, ..before.clone() });
        doc.commit_props(1, before);
        let c = doc.pick(50, 30).unwrap();
        assert!((c[0] as i32 - 128).abs() <= 1 && c[1] == 0 && (c[2] as i32 - 128).abs() <= 1, "{c:?}");
        // 隐藏上层。
        let before = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { visible: false, ..before.clone() });
        doc.commit_props(1, before);
        assert_eq!(pixel(&doc, 50, 30), 0x00FF_0000);
        doc.undo();
        doc.undo();
        assert_eq!(pixel(&doc, 50, 30), 0x0000_00FF);
    }

    #[test]
    fn blend_modes_follow_the_standard_formulas() {
        let mut doc = Document::new(200, 100);
        draw(&mut doc, 30.5, [200, 100, 50]);
        doc.add_layer();
        draw(&mut doc, 30.5, [100, 200, 250]);
        let expect = [
            (BlendMode::Normal, [100, 200, 250]),
            (BlendMode::Multiply, [78, 78, 49]),
            (BlendMode::Screen, [222, 222, 251]),
            (BlendMode::Darken, [100, 100, 50]),
            (BlendMode::Lighten, [200, 200, 250]),
            (BlendMode::Add, [255, 255, 255]),
            (BlendMode::Difference, [100, 100, 200]),
        ];
        for (mode, want) in expect {
            let p = doc.layers()[1].props.clone();
            doc.set_props_live(1, LayerProps { blend: mode, ..p });
            let got = doc.pick(50, 30).unwrap();
            for c in 0..3 {
                assert!((got[c] as i32 - want[c]).abs() <= 1, "{mode:?}: got {got:?}, want {want:?}");
            }
        }
        assert_eq!(BlendMode::from_id("overlay"), Some(BlendMode::Overlay));
        assert_eq!(BlendMode::from_id("nope"), None);
    }

    #[test]
    fn merge_down_keeps_the_picture_and_is_one_undo_step() {
        let mut doc = Document::new(200, 100);
        draw(&mut doc, 30.5, [200, 100, 50]);
        doc.add_layer();
        draw(&mut doc, 30.5, [100, 200, 250]);
        draw(&mut doc, 60.5, [0, 255, 0]);
        let p = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { blend: BlendMode::Multiply, opacity: 0.5, ..p });
        let (a, b) = (doc.pick(50, 30).unwrap(), doc.pick(50, 60).unwrap());
        let depth = doc.undo_depth();
        assert!(doc.merge_down());
        assert_eq!((doc.layers().len(), doc.active()), (1, 0));
        let (a2, b2) = (doc.pick(50, 30).unwrap(), doc.pick(50, 60).unwrap());
        for c in 0..3 {
            assert!((a[c] as i32 - a2[c] as i32).abs() <= 1, "{a:?} vs {a2:?}");
        }
        // 透明处并下去的颜色按"正常"方式落在下层上，与合并前叠在白底上的结果不同是预期之内的：
        // 下层那里是透明的，没有底色可以相乘。只要求它还在、而且是绿色调。
        assert!(b2[1] > b2[0] && b2[1] > b2[2], "{b:?} -> {b2:?}");
        assert_eq!(doc.undo_depth(), depth + 1);
        doc.undo();
        assert_eq!((doc.layers().len(), doc.active()), (2, 1));
        assert_eq!(doc.pick(50, 30).unwrap(), a);
        assert!(!doc.merge_down() || doc.active() == 0);
    }

    /// 混合的结果必须仍是合法的预乘像素：颜色不超过透明度，透明度不超过 1。
    #[test]
    fn blending_keeps_colour_within_alpha() {
        let values = [0u16, 1, 2, 77, 4096, 16383, 16384, 16385, 32767, 32768];
        for mode in BlendMode::ALL {
            for &da in &values {
                for &sa in &values {
                    for &opacity in &[1u32, 16384, ONE] {
                        for (dc, sc) in [(da, sa), (da / 2, sa), (da, sa / 3), (0, sa), (da, 0)] {
                            let out = blend([dc as u32, dc as u32, dc as u32, da as u32], [sc, sc, sc, sa], opacity, mode);
                            assert!(out[3] <= ONE && out[0] <= out[3], "{mode:?} dst=({dc},{da}) src=({sc},{sa}) op={opacity}: {out:?}");
                        }
                    }
                }
            }
        }
        // 审查时发现的具体例子：下层几乎全透明，上层半透明白，滤色模式。
        let out = blend([1, 1, 1, 1], [16384; 4], ONE, BlendMode::Screen);
        assert!(out[0] <= out[3], "{out:?}");
    }

    #[test]
    fn new_layer_names_do_not_repeat_existing_ones() {
        let doc_layers = vec![Layer::new("底稿"), Layer::new("图层 3")];
        let mut doc = Document::from_layers(100, 100, [255, 255, 255], doc_layers);
        doc.add_layer();
        doc.add_layer();
        let names: Vec<&str> = doc.layers().iter().map(|l| l.props.name.as_str()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "{names:?}");
    }

    #[test]
    fn duplicate_remove_and_clear() {
        let mut doc = Document::new(200, 100);
        draw_line(&mut doc, 30.5);
        doc.duplicate_active();
        assert_eq!(doc.layers().len(), 2);
        assert!(doc.layers()[1].props.name.ends_with("副本"));
        assert!(doc.remove_active());
        assert_eq!((doc.layers().len(), doc.active()), (1, 0));
        assert!(!doc.remove_active(), "最后一层不能删");
        assert!(!doc.clear_active().is_empty());
        assert_eq!(pixel(&doc, 50, 30), WHITE);
        doc.undo();
        assert_eq!(pixel(&doc, 50, 30), 0);
        doc.undo();
        assert_eq!(doc.layers().len(), 2);
    }

    #[test]
    fn live_stroke_shows_before_it_is_committed() {
        let doc = Document::new(200, 100);
        let (mut s, _) = Stroke::begin(BrushParams::default(), 0, Sample { x: 10.0, y: 30.5, p: 1.0 });
        s.add(Sample { x: 100.0, y: 30.5, p: 1.0 });
        let mut out = vec![0u32; N];
        doc.composite_tile(Some(&s), (0, 0), &mut out);
        assert_eq!(out[(30 * TILE + 50) as usize], 0);
        doc.composite_tile(None, (0, 0), &mut out);
        assert_eq!(out[(30 * TILE + 50) as usize], WHITE);
    }

    #[test]
    fn strokes_outside_the_canvas_are_clipped() {
        let mut doc = Document::new(64, 64);
        let (mut s, _) = Stroke::begin(BrushParams::default(), 0, Sample { x: 200.0, y: 200.0, p: 1.0 });
        s.end();
        assert!(doc.commit_stroke(&s).is_empty());
        assert!(!doc.is_modified());
    }

    #[test]
    fn png_export_writes_the_composited_canvas() {
        let mut doc = Document::new(200, 100);
        draw_line(&mut doc, 30.5);
        let path = std::env::temp_dir().join(format!("zhibi-export-test-{}.png", std::process::id()));
        doc.export_png(&path).unwrap();
        let dec = png::Decoder::new(std::fs::File::open(&path).unwrap());
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!((info.width, info.height), (200, 100));
        let at = |x: usize, y: usize| &buf[(y * 200 + x) * 3..(y * 200 + x) * 3 + 3];
        assert_eq!(at(50, 30), [0, 0, 0]);
        assert_eq!(at(50, 60), [255, 255, 255]);
    }

    #[test]
    fn eraser_removes_paint() {
        let mut doc = Document::new(200, 100);
        draw_line(&mut doc, 30.5);
        let params = BrushParams { erase: true, size: 30.0, ..Default::default() };
        let (mut s, _) = Stroke::begin(params, 0, Sample { x: 50.0, y: 30.5, p: 1.0 });
        s.end();
        doc.commit_stroke(&s);
        assert_eq!(pixel(&doc, 50, 30), WHITE);
        assert_eq!(pixel(&doc, 90, 30), 0);
    }

    #[test]
    fn selection_limits_painting_and_clearing() {
        use crate::selection::Selection;
        let mut doc = Document::new(200, 100);
        doc.set_selection(Some(Selection::rect(Rect { x0: 40, y0: 0, x1: 60, y1: 100 })));
        draw_line(&mut doc, 30.5);
        assert_eq!(pixel(&doc, 50, 30), 0, "选区里画得上");
        assert_eq!(pixel(&doc, 30, 30), WHITE, "选区外画不上");
        assert_eq!(pixel(&doc, 70, 30), WHITE);
        // 取消选区后整条线画上，再只清掉选区里的一段。
        doc.set_selection(None);
        draw_line(&mut doc, 60.5);
        assert_eq!(pixel(&doc, 30, 60), 0);
        doc.set_selection(Some(Selection::rect(Rect { x0: 40, y0: 50, x1: 60, y1: 70 })));
        assert!(!doc.clear_active().is_empty());
        assert_eq!(pixel(&doc, 50, 60), WHITE);
        assert_eq!(pixel(&doc, 30, 60), 0);
        assert_eq!(pixel(&doc, 50, 30), 0, "选区外的那条线不受影响");
        doc.undo();
        assert_eq!(pixel(&doc, 50, 60), 0);
        // 空选区等同于没有选区。
        doc.set_selection(Some(Selection::empty()));
        assert!(doc.selection().is_none());
    }

    #[test]
    fn replacing_a_whole_layer_is_one_undo_step() {
        let mut doc = Document::new(200, 100);
        draw_line(&mut doc, 30.5);
        let depth = doc.undo_depth();
        let old = doc.set_layer_tiles_live(0, TileMap::default());
        assert_eq!(pixel(&doc, 50, 30), WHITE, "实时预览不进撤销栈");
        assert_eq!(doc.undo_depth(), depth);
        doc.set_layer_tiles_live(0, old);
        doc.replace_layer_tiles(0, TileMap::default());
        assert_eq!(pixel(&doc, 50, 30), WHITE);
        assert_eq!(doc.undo_depth(), depth + 1);
        doc.undo();
        assert_eq!(pixel(&doc, 50, 30), 0);
        doc.redo();
        assert_eq!(pixel(&doc, 50, 30), WHITE);
    }

    #[test]
    fn selection_follows_undo_when_replaced_together_with_pixels() {
        use crate::selection::Selection;
        let mut doc = Document::new(200, 100);
        draw_line(&mut doc, 30.5);
        let here = Rect { x0: 40, y0: 20, x1: 60, y1: 40 };
        let there = Rect { x0: 100, y0: 20, x1: 120, y1: 40 };
        doc.set_selection(Some(Selection::rect(here)));
        doc.replace_layer_tiles_and_selection(0, TileMap::default(), Some(Selection::rect(there)));
        assert_eq!(doc.selection().unwrap().bounds(), there);
        doc.undo();
        assert_eq!(doc.selection().unwrap().bounds(), here);
        assert_eq!(pixel(&doc, 50, 30), 0);
        doc.redo();
        assert_eq!(doc.selection().unwrap().bounds(), there);
        // 只换像素的步骤不动选区。
        doc.replace_layer_tiles(0, TileMap::default());
        doc.set_selection(None);
        doc.undo();
        assert!(doc.selection().is_none());
    }

    /// 某一层在 (x, y) 处的像素（预乘，15 位）。
    fn raw(doc: &Document, layer: usize, x: i32, y: i32) -> [u16; 4] {
        let i = ((y % TILE) * TILE + x % TILE) as usize * 4;
        doc.layers()[layer].tiles.get((x / TILE, y / TILE)).map_or([0; 4], |t| [t.0[i], t.0[i + 1], t.0[i + 2], t.0[i + 3]])
    }

    /// 画面上第 x 列从上到下的颜色。
    fn column(doc: &Document, x: i32) -> Vec<u32> {
        (0..doc.height).map(|y| pixel(doc, x, y)).collect()
    }

    /// 两列颜色看起来一样：每个通道最多差 1（中间多一次四舍五入）。
    fn same(a: &[u32], b: &[u32]) {
        for (a, b) in a.iter().zip(b) {
            for shift in [0, 8, 16] {
                assert!(((a >> shift) & 0xFF).abs_diff((b >> shift) & 0xFF) <= 1, "{a:06X} {b:06X}");
            }
        }
    }

    #[test]
    fn merging_visible_layers_keeps_the_picture_and_the_hidden_ones() {
        let mut doc = Document::new(200, 100);
        draw(&mut doc, 20.5, [0, 0, 0]);
        doc.add_layer();
        draw(&mut doc, 40.5, [255, 0, 0]);
        let p = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { visible: false, ..p });
        doc.add_layer();
        draw(&mut doc, 20.5, [0, 0, 255]);
        let p = doc.layers()[2].props.clone();
        doc.set_props_live(2, LayerProps { opacity: 0.5, ..p });
        let before = column(&doc, 50);
        let depth = doc.undo_depth();

        assert!(doc.merge_visible());
        assert_eq!(doc.layers().len(), 2);
        assert_eq!(doc.active(), 0);
        assert!(!doc.layers()[1].props.visible, "隐藏的那一层还在");
        same(&column(&doc, 50), &before);
        assert_eq!(doc.undo_depth(), depth + 1);
        doc.undo();
        assert_eq!(doc.layers().len(), 3);
        assert_eq!(doc.layers()[2].props.opacity, 0.5);
        assert_eq!(column(&doc, 50), before);
        doc.redo();
        assert_eq!(doc.layers().len(), 2);
        // 只剩一个可见图层时没有可合并的。
        assert!(!doc.merge_visible());
    }

    #[test]
    fn flattening_bakes_in_the_background_and_drops_hidden_layers() {
        let mut doc = Document::new(130, 70);
        doc.background = [200, 220, 240];
        draw(&mut doc, 20.5, [0, 0, 0]);
        doc.add_layer();
        draw(&mut doc, 40.5, [255, 0, 0]);
        let p = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { visible: false, ..p });
        let before = column(&doc, 50);
        doc.flatten();
        assert_eq!(doc.layers().len(), 1);
        same(&column(&doc, 50), &before);
        assert_eq!(raw(&doc, 0, 129, 69)[3] as u32, ONE, "整张都是不透明的，包括不满一块的边角");
        assert_eq!(pixel(&doc, 50, 40), 0x00C8_DCF0, "隐藏图层上的红线没有进来");
        doc.undo();
        assert_eq!(doc.layers().len(), 2);
        assert_eq!(raw(&doc, 0, 129, 69)[3], 0);
    }

    #[test]
    fn filling_respects_the_selection() {
        use crate::selection::Selection;
        let mut doc = Document::new(130, 70);
        assert!(!doc.fill_active([10, 200, 30]).is_empty());
        assert_eq!(pixel(&doc, 0, 0), 0x000A_C81E);
        assert_eq!(pixel(&doc, 129, 69), 0x000A_C81E);
        doc.set_selection(Some(Selection::rect(Rect { x0: 60, y0: 10, x1: 70, y1: 20 })));
        doc.fill_active([255, 0, 0]);
        assert_eq!(pixel(&doc, 65, 15), 0x00FF_0000);
        assert_eq!(pixel(&doc, 59, 15), 0x000A_C81E);
        doc.undo();
        assert_eq!(pixel(&doc, 65, 15), 0x000A_C81E);
        doc.undo();
        assert_eq!(pixel(&doc, 65, 15), WHITE);
    }

    #[test]
    fn white_becomes_transparent_without_changing_the_look_on_white() {
        let mut doc = Document::new(200, 100);
        doc.fill_active([255, 255, 255]);
        draw(&mut doc, 30.5, [0, 0, 0]);
        draw(&mut doc, 60.5, [40, 90, 200]);
        let before = column(&doc, 50);
        assert_eq!(raw(&doc, 0, 50, 5)[3] as u32, ONE);
        assert!(!doc.white_to_alpha().is_empty());
        assert_eq!(raw(&doc, 0, 50, 5), [0; 4], "白底变透明了");
        assert_eq!(raw(&doc, 0, 50, 30)[3] as u32, ONE, "黑线还是实的");
        same(&column(&doc, 50), &before);
        doc.undo();
        assert_eq!(raw(&doc, 0, 50, 5)[3] as u32, ONE);
        // 空图层上没有可处理的。
        doc.add_layer();
        assert!(doc.white_to_alpha().is_empty());
    }

    #[test]
    fn protecting_opacity_only_recolours_existing_pixels() {
        let mut doc = Document::new(200, 100);
        draw(&mut doc, 30.5, [0, 0, 0]);
        let p = doc.layers()[0].props.clone();
        doc.set_props_live(0, LayerProps { lock_alpha: true, ..p });
        // 竖着画一道红的，穿过那条黑线：只有黑线上的那一段变红。
        let (mut s, _) = Stroke::begin(BrushParams { color: [255, 0, 0], ..Default::default() }, 0, Sample { x: 50.0, y: 5.0, p: 1.0 });
        s.add(Sample { x: 50.0, y: 90.0, p: 1.0 });
        doc.composite_tile(Some(&s), (0, 0), &mut vec![0u32; N]);
        s.end();
        let alpha_before = raw(&doc, 0, 50, 30)[3];
        doc.commit_stroke(&s);
        assert_eq!(pixel(&doc, 50, 30), 0x00FF_0000);
        assert_eq!(pixel(&doc, 50, 60), WHITE, "透明的地方画不上");
        assert_eq!(raw(&doc, 0, 50, 30)[3], alpha_before);
        // 橡皮擦不掉，清除也不行，填充只换颜色。
        let (mut e, _) = Stroke::begin(BrushParams { erase: true, size: 30.0, ..Default::default() }, 0, Sample { x: 20.0, y: 30.0, p: 1.0 });
        e.add(Sample { x: 80.0, y: 30.0, p: 1.0 });
        e.end();
        doc.commit_stroke(&e);
        assert_eq!(pixel(&doc, 50, 30), 0x00FF_0000);
        assert!(doc.clear_active().is_empty());
        doc.fill_active([0, 0, 255]);
        assert_eq!(pixel(&doc, 50, 30), 0x0000_00FF);
        assert_eq!(pixel(&doc, 50, 60), WHITE);
        assert!(doc.white_to_alpha().is_empty());
    }

    #[test]
    fn a_locked_layer_cannot_be_changed() {
        let mut doc = Document::new(200, 100);
        draw_line(&mut doc, 30.5);
        let p = doc.layers()[0].props.clone();
        doc.set_props_live(0, LayerProps { locked: true, ..p.clone() });
        assert!(!doc.can_edit_active());
        let depth = doc.undo_depth();
        draw_line(&mut doc, 60.5);
        assert!(doc.clear_active().is_empty());
        assert!(doc.fill_active([255, 0, 0]).is_empty());
        assert!(doc.replace_layer_tiles(0, TileMap::default()).is_empty());
        assert_eq!(doc.undo_depth(), depth);
        assert_eq!(pixel(&doc, 50, 30), 0);
        assert_eq!(pixel(&doc, 50, 60), WHITE);
        // 往锁定的图层上合并也不行。
        doc.add_layer();
        assert!(!doc.merge_down());
        doc.set_active(0);
        doc.set_props_live(0, LayerProps { visible: false, ..p });
        assert!(!doc.can_edit_active(), "看不见的图层也不让画");
    }

    #[test]
    fn a_clipped_layer_shows_only_over_the_layer_below() {
        let mut doc = Document::new(200, 100);
        draw_line(&mut doc, 30.5);
        doc.add_layer();
        doc.fill_active([255, 0, 0]);
        assert_eq!(pixel(&doc, 50, 60), 0x00FF_0000);
        let p = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { clip: true, ..p.clone() });
        assert_eq!(pixel(&doc, 50, 30), 0x00FF_0000, "底下有线的地方显示红色");
        assert_eq!(pixel(&doc, 50, 60), WHITE, "底下是空的地方不显示");
        assert_eq!(pixel(&doc, 150, 30), WHITE, "底在这个分块上没有内容");
        // 底隐藏时，剪贴在它上面的也跟着不显示。
        let base = doc.layers()[0].props.clone();
        doc.set_props_live(0, LayerProps { visible: false, ..base.clone() });
        assert_eq!(pixel(&doc, 50, 30), WHITE);
        doc.set_props_live(0, base);
        // 正在底上画的一笔立刻成为遮罩的一部分。
        doc.set_active(0);
        let (mut s, _) = Stroke::begin(BrushParams::default(), 0, Sample { x: 10.0, y: 60.5, p: 1.0 });
        s.add(Sample { x: 100.0, y: 60.5, p: 1.0 });
        let mut out = vec![0u32; N];
        doc.composite_tile(Some(&s), (0, 0), &mut out);
        assert_eq!(out[(60 * TILE + 50) as usize], 0x00FF_0000);
        // 向下合并：并进去的只是露出来的那部分。
        doc.set_active(1);
        let before = column(&doc, 50);
        assert!(doc.merge_down());
        same(&column(&doc, 50), &before);
        assert_eq!(raw(&doc, 0, 50, 60), [0; 4]);
        doc.undo();
        // 合并可见图层和拼合也按看到的样子来。
        assert!(doc.merge_visible());
        same(&column(&doc, 50), &before);
        assert!(!doc.layers()[0].props.clip);
        // 最下面一层没有底可剪，剪贴开着也照常显示。
        let p = doc.layers()[0].props.clone();
        doc.set_props_live(0, LayerProps { clip: true, ..p });
        assert_eq!(pixel(&doc, 50, 30), 0x00FF_0000);
    }

    /// 用马克笔画一笔：`points` 是折线的各个顶点。
    fn marker_stroke(doc: &mut Document, points: &[(f32, f32)], color: [u8; 3], density: f32) {
        let params = BrushParams { size: 20.0, density, color, marker: true, ..Default::default() };
        let (mut s, _) = Stroke::begin(params, 0, Sample { x: points[0].0, y: points[0].1, p: 1.0 });
        for p in &points[1..] {
            s.add(Sample { x: p.0, y: p.1, p: 1.0 });
        }
        s.end();
        doc.commit_stroke(&s);
    }

    fn valid(doc: &Document, layer: usize) -> bool {
        let tiles = &doc.layers()[layer].tiles;
        tiles.coords().all(|t| {
            tiles.get(t).unwrap().0.as_chunks::<4>().0.iter().all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3] && p[3] as u32 <= ONE)
        })
    }

    #[test]
    fn marker_strokes_do_not_darken_where_they_cross() {
        let mut doc = Document::new(200, 100);
        doc.add_layer();
        marker_stroke(&mut doc, &[(20.0, 30.0), (180.0, 30.0)], [200, 60, 60], 0.5);
        let single = pixel(&doc, 60, 30);
        assert_ne!(single, WHITE);
        // 另起一笔同色竖着穿过去：交叉处和单独一笔一样深。
        marker_stroke(&mut doc, &[(100.0, 5.0), (100.0, 90.0)], [200, 60, 60], 0.5);
        assert_eq!(pixel(&doc, 100, 30), single);
        assert_eq!(pixel(&doc, 100, 70), single);
        // 不透明度停在这一笔的浓度上。
        assert!((raw(&doc, 1, 100, 30)[3] as i32 - (ONE / 2) as i32).abs() <= 2, "{:?}", raw(&doc, 1, 100, 30));
        // 更浓的一笔才能把它提上去。
        marker_stroke(&mut doc, &[(20.0, 30.0), (180.0, 30.0)], [200, 60, 60], 1.0);
        assert_eq!(raw(&doc, 1, 60, 30)[3] as u32, ONE);
        assert!(valid(&doc, 1));
    }

    #[test]
    fn marker_colour_moves_towards_the_new_colour() {
        let mut doc = Document::new(200, 100);
        doc.add_layer();
        marker_stroke(&mut doc, &[(20.0, 30.0), (180.0, 30.0)], [20, 40, 220], 1.0);
        // 空白处满浓度的一笔就是笔色本身。
        assert_eq!(pixel(&doc, 60, 30), 0x0014_28DC);
        let blue = raw(&doc, 1, 100, 30);
        // 半浓度的红色叠上去：不透明度不降，颜色往红色走了一段，还没到底。
        marker_stroke(&mut doc, &[(100.0, 5.0), (100.0, 90.0)], [230, 20, 20], 0.5);
        let mixed = raw(&doc, 1, 100, 30);
        assert_eq!(mixed[3], blue[3]);
        assert!(mixed[0] > blue[0] && mixed[2] < blue[2], "{blue:?} -> {mixed:?}");
        assert!(mixed[2] as u32 > ONE / 8, "{mixed:?}");
        // 满浓度的红色叠上去：完全换成红色。
        marker_stroke(&mut doc, &[(100.0, 5.0), (100.0, 90.0)], [230, 20, 20], 1.0);
        assert_eq!(pixel(&doc, 100, 30), 0x00E6_1414);
        assert!(valid(&doc, 1));
    }

    #[test]
    fn marker_with_protected_opacity_keeps_the_opacity() {
        let mut doc = Document::new(200, 100);
        doc.add_layer();
        marker_stroke(&mut doc, &[(20.0, 30.0), (180.0, 30.0)], [240, 200, 120], 0.6);
        let p = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { lock_alpha: true, ..p });
        let before = raw(&doc, 1, 100, 30);
        marker_stroke(&mut doc, &[(100.0, 5.0), (100.0, 90.0)], [30, 30, 230], 1.0);
        let after = raw(&doc, 1, 100, 30);
        assert_eq!(after[3], before[3], "不透明度不变");
        assert!(after[2] > before[2] && after[0] < before[0], "{before:?} -> {after:?}");
        assert_eq!(pixel(&doc, 100, 70), WHITE, "透明的地方还是透明");
        assert!(valid(&doc, 1));
    }

    #[test]
    fn one_marker_stroke_made_of_many_small_steps_is_even() {
        let mut doc = Document::new(400, 100);
        doc.add_layer();
        // 鼠标或笔一路送来很多小步：一笔之内各处的深浅应当一样，接头处不能有深色的点。
        let points: Vec<(f32, f32)> = (0..=60).map(|k| (20.0 + 6.0 * k as f32, 50.0 + if k % 2 == 0 { 0.0 } else { 0.7 })).collect();
        marker_stroke(&mut doc, &points, [200, 60, 60], 0.5);
        let line: Vec<u32> = (40..360).map(|x| pixel(&doc, x, 50)).collect();
        assert!(line.iter().all(|&c| c == line[0]), "{:06X?}", line.iter().filter(|&&c| c != line[0]).take(5).collect::<Vec<_>>());
    }
}
