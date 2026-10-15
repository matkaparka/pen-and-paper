// SPDX-License-Identifier: GPL-3.0-or-later
//! 图层结构：新建、删除、复制、挪动、文件夹（规格 F-055，实现方案 §3.11），合并，
//! 蒙版的添加、删除、应用、停用（规格 F-058），填充图层的改色和栅格化（规格 F-061）。
//!
//! 图层是一个自下而上的列表，每层记着嵌套深度。文件夹里面的东西是紧挨在它下面、深度比它大的那一串。
//! 下面说的"上""下"都是图层列表上看到的方向，也就是序号的大和小。

use std::ops::Range;
use std::sync::Arc;

use super::*;

impl Document {
    /// 第 i 层连同它里面的东西（是文件夹的话）在列表里占的范围。
    pub fn subtree(&self, i: usize) -> Range<usize> {
        let d = self.layers[i].depth;
        let mut a = i;
        if self.layers[i].is_folder() {
            while a > 0 && self.layers[a - 1].depth > d {
                a -= 1;
            }
        }
        a..i + 1
    }

    /// 包着第 i 层的文件夹，由里到外。
    pub fn ancestors(&self, i: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut d = self.layers[i].depth;
        for j in i + 1..self.layers.len() {
            if d == 0 {
                break;
            }
            if self.layers[j].depth < d {
                out.push(j);
                d = self.layers[j].depth;
            }
        }
        out
    }

    /// 第 i 层在图层列表里露得出来吗：包着它的文件夹都展开着。
    pub fn listed(&self, i: usize) -> bool {
        self.ancestors(i).into_iter().all(|j| matches!(self.layers[j].kind, Kind::Folder { open: true }))
    }

    /// 展开或者折叠一个文件夹。只是图层列表的显示，不记撤销。
    pub fn set_folder_open(&mut self, i: usize, open: bool) {
        if let Some(Kind::Folder { open: o }) = self.layers.get_mut(i).map(|l| &mut l.kind) {
            *o = open;
        }
    }

    /// 新建的东西放在哪里：当前是展开的文件夹，放进它里面的最上面；否则放在当前图层（连同它里面的东西）上面。
    fn new_slot(&self) -> (usize, u16) {
        let a = &self.layers[self.active];
        match a.kind {
            Kind::Folder { open: true } => (self.active, a.depth + 1),
            _ => (self.active + 1, a.depth),
        }
    }

    fn next_layer_name(&mut self) -> String {
        // 编号一直往上数，并跳过已经被用掉的名字（读入的文件里可能已有"图层 5"）。
        loop {
            self.layer_counter += 1;
            let name = trf!("图层 {}", "Layer {}", self.layer_counter);
            if self.layers.iter().all(|l| l.props.name != name) {
                return name;
            }
        }
    }

    fn next_folder_name(&mut self) -> String {
        loop {
            self.folder_counter += 1;
            let name = trf!("组 {}", "Group {}", self.folder_counter);
            if self.layers.iter().all(|l| l.props.name != name) {
                return name;
            }
        }
    }

    /// 新建图层并选中它（规格 F-050）。
    pub fn add_layer(&mut self) -> usize {
        let name = self.next_layer_name();
        let (index, depth) = self.new_slot();
        let mut layer = Layer::new(&name);
        layer.depth = depth;
        self.insert_layer(index, layer)
    }

    /// 新建一个空文件夹，放在当前图层上面（规格 F-055）。
    pub fn add_folder(&mut self) -> usize {
        let name = self.next_folder_name();
        let mut folder = Layer::folder(&name);
        folder.depth = self.layers[self.active].depth;
        self.insert_layer(self.active + 1, folder)
    }

    fn insert_layer(&mut self, index: usize, layer: Layer) -> usize {
        let before = self.active;
        self.layers.insert(index, layer);
        self.active = index;
        self.mask_target = false;
        self.push(Action::Presence { index, layer: None }, before);
        index
    }

    /// 把现成的一层放在新建图层的位置上并选中它，一个撤销步骤（导入为新图层，规格 F-148）。返回它的位置。
    pub fn add_layer_with(&mut self, mut layer: Layer) -> usize {
        let (index, depth) = self.new_slot();
        layer.depth = depth;
        self.insert_layer(index, layer)
    }

    /// 放进新文件夹（Ctrl+G）：在当前图层的位置建一个文件夹，把当前图层（连同它里面的东西）放进去。
    pub fn group_active(&mut self) -> usize {
        let r = self.subtree(self.active);
        let before = self.active;
        let name = self.next_folder_name();
        let mut folder = Layer::folder(&name);
        folder.depth = self.layers[self.active].depth;
        self.layers.insert(r.end, folder);
        let mut actions = vec![Action::Presence { index: r.end, layer: None }];
        let order: Vec<usize> = (0..self.layers.len()).collect();
        let depths = self.layers.iter().enumerate().map(|(k, l)| l.depth + r.contains(&k) as u16).collect();
        actions.push(self.arrange(order, depths));
        self.active = r.end;
        self.mask_target = false;
        self.push(Action::Group(actions), before);
        r.end
    }

    /// 复制当前图层（是文件夹的话连里面的一起），放在它上面并选中。
    pub fn duplicate_active(&mut self) -> usize {
        let r = self.subtree(self.active);
        let before = self.active;
        let mut copies = self.layers[r.clone()].to_vec();
        if let Some(top) = copies.last_mut() {
            top.props.name = trf!("{} 副本", "{} copy", top.props.name);
        }
        let mut actions = Vec::new();
        for (k, layer) in copies.into_iter().enumerate() {
            self.layers.insert(r.end + k, layer);
            actions.push(Action::Presence { index: r.end + k, layer: None });
        }
        self.active = r.end + r.len() - 1;
        self.mask_target = false;
        self.push(Action::Group(actions), before);
        self.active
    }

    /// 删除当前图层（是文件夹的话连里面的一起）。会把图层删光时不删。
    pub fn remove_active(&mut self) -> bool {
        let r = self.subtree(self.active);
        if r.len() >= self.layers.len() {
            return false;
        }
        let before = self.active;
        let mut actions = Vec::new();
        for i in r.clone().rev() {
            let layer = self.layers.remove(i);
            actions.push(Action::Presence { index: i, layer: Some(Box::new(layer)) });
        }
        self.active = r.start.saturating_sub(1).min(self.layers.len() - 1);
        self.mask_target = false;
        self.push(Action::Group(actions), before);
        true
    }

    /// 按给出的新顺序（第 k 个位置放现在的第 `order[k]` 层）和深度重新排列，返回撤销用的动作（还没记进撤销栈）。
    fn arrange(&mut self, order: Vec<usize>, depths: Vec<u16>) -> Action {
        let mut action = Action::Arrange { order, depths };
        Self::apply(&mut self.layers, &mut action, false);
        action
    }

    /// 按 `order` 重排、`moved` 那一串深度变 `shift` 之后各层的深度。挪完不合文件夹的规矩、或者根本没动时返回 None。
    fn rearranged(&self, order: &[usize], moved: &Range<usize>, shift: i32) -> Option<Vec<u16>> {
        let depths: Vec<u16> = order
            .iter()
            .map(|&i| (self.layers[i].depth as i32 + if moved.contains(&i) { shift } else { 0 }).max(0) as u16)
            .collect();
        let valid = valid_depths(&order.iter().map(|&i| self.layers[i].is_folder()).collect::<Vec<_>>(), &depths);
        let unchanged = order.iter().enumerate().all(|(k, &i)| k == i) && shift == 0;
        (valid && !unchanged).then_some(depths)
    }

    /// 挪动当前图层（连同它里面的东西）：新的顺序是 `order`，挪动的那一串深度变 `shift`。记一个撤销步骤。
    fn rearrange(&mut self, order: Vec<usize>, moved: Range<usize>, shift: i32) -> bool {
        let Some(depths) = self.rearranged(&order, &moved, shift) else { return false };
        let before = self.active;
        let root = moved.end - 1;
        let to = order.iter().position(|&i| i == root).unwrap_or(root);
        let action = self.arrange(order, depths);
        self.active = to;
        self.push(action, before);
        true
    }

    /// 上移（delta 为正）或下移当前图层（实现方案 §3.11）：已经是所在文件夹里最上面（最下面）的就移出文件夹；
    /// 挨着的兄弟是展开的文件夹就进到它里面；否则和挨着的兄弟（连同它里面的东西）对调。
    pub fn move_active(&mut self, delta: i32) -> bool {
        let n = self.layers.len();
        let r = self.subtree(self.active);
        let d = self.layers[self.active].depth;
        let same: Vec<usize> = (0..n).collect();
        let (order, shift) = if delta > 0 {
            match self.layers.get(r.end) {
                None => return false,
                // 上面挨着的是包着它的文件夹：移出来，放到文件夹上面。
                Some(above) if above.depth < d => {
                    let p = r.end;
                    let mut o: Vec<usize> = (0..r.start).collect();
                    o.extend(r.end..=p);
                    o.extend(r.clone());
                    o.extend(p + 1..n);
                    (o, -1)
                }
                Some(_) => {
                    let s = (r.end..n).find(|&j| self.layers[j].depth <= d).unwrap_or(n - 1);
                    if matches!(self.layers[s].kind, Kind::Folder { open: true }) {
                        // 进到展开的文件夹里面的最下面：位置不变，深一层。
                        (same, 1)
                    } else {
                        let mut o: Vec<usize> = (0..r.start).collect();
                        o.extend(r.end..=s);
                        o.extend(r.clone());
                        o.extend(s + 1..n);
                        (o, 0)
                    }
                }
            }
        } else if delta < 0 {
            if r.start == 0 || self.layers[r.start - 1].depth < d {
                // 已经是最下面的：在文件夹里的话移出来（位置不变，浅一层）。
                if d == 0 {
                    return false;
                }
                (same, -1)
            } else {
                let s = r.start - 1;
                let sr = self.subtree(s);
                if matches!(self.layers[s].kind, Kind::Folder { open: true }) {
                    // 进到下面那个展开的文件夹里面的最上面。
                    let mut o: Vec<usize> = (0..s).collect();
                    o.extend(r.clone());
                    o.push(s);
                    o.extend(r.end..n);
                    (o, 1)
                } else {
                    let mut o: Vec<usize> = (0..sr.start).collect();
                    o.extend(r.clone());
                    o.extend(sr.clone());
                    o.extend(r.end..n);
                    (o, 0)
                }
            }
        } else {
            return false;
        };
        self.rearrange(order, r, shift)
    }

    /// 把当前图层（连同它里面的东西）挪到第 `gap` 个空隙（0 是最下面，图层个数是最上面），深度为 `depth`。
    /// 拖动排序用（实现方案 §3.11）。挪进自己里面、或者挪完不合文件夹的规矩时什么也不做。
    pub fn move_active_to(&mut self, gap: usize, depth: u16) -> bool {
        let Some((order, r, shift)) = self.plan_move(gap, depth) else { return false };
        self.rearrange(order, r, shift)
    }

    /// `move_active_to` 会不会真的挪动（拖动时用来决定要不要画提示）。
    pub fn can_move_active_to(&self, gap: usize, depth: u16) -> bool {
        self.plan_move(gap, depth).is_some_and(|(order, r, shift)| self.rearranged(&order, &r, shift).is_some())
    }

    /// 挪到第 `gap` 个空隙、深度 `depth` 时的新顺序、挪动的范围和深度的变化。挪进自己里面时返回 None。
    fn plan_move(&self, gap: usize, depth: u16) -> Option<(Vec<usize>, Range<usize>, i32)> {
        let n = self.layers.len();
        let r = self.subtree(self.active);
        if gap > n || (gap > r.start && gap < r.end) {
            return None;
        }
        let rest: Vec<usize> = (0..n).filter(|i| !r.contains(i)).collect();
        let at = rest.iter().filter(|&&i| i < gap).count();
        let mut order = rest[..at].to_vec();
        order.extend(r.clone());
        order.extend(&rest[at..]);
        let shift = depth as i32 - self.layers[self.active].depth as i32;
        Some((order, r, shift))
    }

    // ---- 合并 ----

    /// 把 `range` 里的东西（只算看得见的）合成一张图，只算画布里。`base` 是垫在最下面的颜色，None 表示透明。
    fn stack_range(&self, range: Range<usize>, base: Option<[u32; 4]>) -> TileMap {
        self.stack_range_in(range, base, self.bounds())
    }

    /// 同 `stack_range`，范围是 `bounds`（向下合并一个组时连画布外的一起算）。
    fn stack_range_in(&self, range: Range<usize>, base: Option<[u32; 4]>, bounds: Rect) -> TileMap {
        let live = Live::new(self, (0, 0), None);
        let pick = |i: usize| self.layers[i].props.visible;
        let has_fill = range.clone().any(|i| matches!(self.layers[i].kind, Kind::Fill(_)));
        let mut out = TileMap::default();
        for tc in tiles_in(bounds) {
            if base.is_none() && !has_fill && range.clone().all(|i| self.layers[i].tiles.get(tc).is_none()) {
                continue;
            }
            let mut acc = vec![base.unwrap_or([0; 4]); N];
            self.level(tc, &mut acc, range.clone(), &pick, &live, false);
            if acc.iter().all(|p| p[3] == 0) {
                continue;
            }
            // 分块伸出画布的那部分不留东西。
            let (whole, clip) = (tile_rect(tc), tile_rect(tc).intersect(bounds));
            let d = out.get_mut(tc);
            for y in clip.y0..clip.y1 {
                for x in clip.x0..clip.x1 {
                    let i = ((y - whole.y0) * TILE + (x - whole.x0)) as usize;
                    for (c, v) in acc[i].iter().enumerate() {
                        d.0[i * 4 + c] = *v as u16;
                    }
                }
            }
        }
        out
    }

    /// 向下合并（规格 F-052）：把当前图层并入它下面紧挨着的兄弟，结果保留下层的属性。两层的蒙版先作用到像素上。
    /// 当前是文件夹时，把整个文件夹合成一个像素图层（实现方案 §3.11）。
    pub fn merge_down(&mut self) -> bool {
        let upper = self.active;
        if !self.can_merge_down() {
            return false;
        }
        if self.layers[upper].is_folder() {
            return self.merge_folder();
        }
        let lower = upper - 1;
        let styled = |l: &Layer| l.style.as_ref().is_some_and(|s| s.shows());
        // 带样式的、"透明形状图层"关着的 PS 那八种模式：按显示时的合成方法合并，结果和合并前看起来一样。
        let shapeless = !self.layers[upper].props.shapes && neutral(self.layers[upper].props.blend).is_some();
        if styled(&self.layers[upper]) || styled(&self.layers[lower]) || shapeless {
            return self.merge_styled(upper, lower);
        }
        let src = &self.layers[upper];
        let dst = &self.layers[lower];
        // 上层剪贴在下层上时，合并进去的只是它露出来的那部分。
        let masked = src.props.clip && !dst.props.clip;
        let opacity = (src.props.opacity.clamp(0.0, 1.0) * ONE as f32) as u32;
        let upper_fill = (src.props.fill.clamp(0.0, 1.0) * ONE as f32).round() as u32;
        let src_fill = match &src.kind {
            Kind::Fill(f) if f.paint != Paint::Other => Some(fill_pixels(f)),
            _ => None,
        };
        let lower_mask = dst.mask.as_ref().filter(|m| !m.disabled);
        // 两层伸出画布的内容都留着（实现方案 §3.15）；纯色填充只铺画布。
        let keep = self.keep_bounds();
        let mut coords: Vec<TileCoord> = src.tiles.coords().chain(dst.tiles.coords()).filter(|&tc| !tile_rect(tc).intersect(keep).is_empty()).collect();
        if src_fill.is_some() {
            coords.extend(tiles_in(self.bounds()));
        }
        coords.sort_unstable();
        coords.dedup();
        let mut merged = TileMap::default();
        for tc in coords {
            let s_tile = src.tiles.get(tc);
            let d_tile = dst.tiles.get(tc);
            if s_tile.is_none() && src_fill.is_none() && d_tile.is_none() {
                continue;
            }
            let s_mask = src.mask.as_ref().filter(|m| !m.disabled).map(|m| m.tile(tc).map_or_else(|| vec![m.default; N], |t| t.to_vec()));
            let d_mask = lower_mask.map(|m| m.tile(tc).map_or_else(|| vec![m.default; N], |t| t.to_vec()));
            let mut out = vec![0u16; N * 4];
            let mut any = false;
            for i in 0..N {
                let mut dp = d_tile.map_or([0u32; 4], |t| [t.0[i * 4] as u32, t.0[i * 4 + 1] as u32, t.0[i * 4 + 2] as u32, t.0[i * 4 + 3] as u32]);
                if let Some(m) = &d_mask {
                    dp = dp.map(|v| mul15(v, m[i] as u32));
                }
                let mut sp = match (&src_fill, s_tile) {
                    (Some(f), _) => f[i],
                    (None, Some(t)) => [t.0[i * 4], t.0[i * 4 + 1], t.0[i * 4 + 2], t.0[i * 4 + 3]],
                    (None, None) => [0; 4],
                };
                if let Some(m) = &s_mask {
                    sp = sp.map(|v| mul15(v as u32, m[i] as u32) as u16);
                }
                sp = with_fill(sp, upper_fill, src.props.blend);
                // 剪贴的：和显示时一样按 source-atop 叠在下层上（实现方案 §3.22），透明度还是下层的。
                let px = if sp[3] == 0 {
                    dp
                } else if masked {
                    blend_atop(dp, sp, opacity, src.props.blend)
                } else {
                    blend(dp, sp, opacity, src.props.blend)
                };
                any |= px[3] != 0;
                for c in 0..4 {
                    out[i * 4 + c] = px[c] as u16;
                }
            }
            if any {
                merged.replace(tc, Some(Arc::new(Tile(out))));
            }
        }
        let mut new_lower = dst.clone();
        new_lower.tiles = merged;
        if lower_mask.is_some() {
            new_lower.mask = None;
        }
        let before = self.active;
        let removed = self.layers.remove(upper);
        let old_lower = std::mem::replace(&mut self.layers[lower], new_lower);
        self.active = lower;
        self.mask_target = false;
        self.push(
            Action::Group(vec![
                Action::Presence { index: upper, layer: Some(Box::new(removed)) },
                Action::Swap { index: lower, layer: Box::new(old_lower) },
            ]),
            before,
        );
        true
    }

    /// 向下合并时两层里有带样式的（或者上层是"透明形状图层"关着的那八种模式）：两层单独合成一张图
    /// （效果画进去，和 PS 一样），结果保留下层的属性，
    /// 蒙版、样式都已经画进去，去掉。下层自己的不透明度、混合模式不画进去，照旧作为属性留着。
    fn merge_styled(&mut self, upper: usize, lower: usize) -> bool {
        let mut a = self.layers[lower].clone();
        a.props = LayerProps { opacity: 1.0, fill: 1.0, shapes: true, blend: BlendMode::Normal, visible: true, clip: false, ..a.props.clone() };
        a.depth = 0;
        let mut b = self.layers[upper].clone();
        b.depth = 0;
        b.props.visible = true;
        let area = [&a, &b].iter().fold(self.bounds(), |r, l| l.tiles.coords().fold(r, |r, tc| r.union(tile_rect(tc))));
        let pair = Document::from_layers(self.width, self.height, self.background, vec![a, b]);
        let tiles = pair.stack_range_in(0..2, None, area.intersect(self.keep_bounds()));
        let mut new_lower = self.layers[lower].clone();
        new_lower.tiles = tiles;
        new_lower.kind = Kind::Pixels;
        new_lower.mask = None;
        new_lower.style = None;
        let before = self.active;
        let removed = self.layers.remove(upper);
        let old_lower = std::mem::replace(&mut self.layers[lower], new_lower);
        self.active = lower;
        self.mask_target = false;
        self.push(
            Action::Group(vec![
                Action::Presence { index: upper, layer: Some(Box::new(removed)) },
                Action::Swap { index: lower, layer: Box::new(old_lower) },
            ]),
            before,
        );
        true
    }

    /// 打开或关掉当前图层的样式（规格 F-062）。没有样式、或者锁定时什么也不做。
    pub fn toggle_style(&mut self) -> bool {
        let i = self.active;
        if self.is_locked(i) {
            return false;
        }
        let Some(mut style) = self.layers[i].style.clone() else { return false };
        style.set_enabled(!style.enabled);
        let old = self.layers[i].style.replace(style);
        let active = self.active;
        self.push(Action::Style { index: i, style: old }, active);
        true
    }

    /// 能不能向下合并：当前是没锁的文件夹；或者下面紧挨着的是同一层级、没锁的像素图层。
    pub fn can_merge_down(&self) -> bool {
        let a = self.active;
        let l = &self.layers[a];
        if l.is_folder() {
            return !self.is_locked(a);
        }
        a > 0 && self.layers[a - 1].depth == l.depth && self.layers[a - 1].is_pixels() && !self.is_locked(a - 1)
    }

    /// 把当前的文件夹合成一个像素图层：里面看得见的东西按隔离的方式合成，名字、不透明度、蒙版、位置不变；
    /// 穿透的文件夹合成后混合模式是"正常"。
    fn merge_folder(&mut self) -> bool {
        let g = self.active;
        if self.is_locked(g) {
            return false;
        }
        let r = self.subtree(g);
        // 组里的东西伸出画布的部分也留着：范围是里面各层分块的范围（有填充图层时再加上画布）。
        let mut area = Rect::EMPTY;
        for j in r.start..g {
            let l = &self.layers[j];
            area = l.tiles.coords().fold(area, |a, tc| a.union(tile_rect(tc)));
            if matches!(l.kind, Kind::Fill(_)) {
                area = area.union(self.bounds());
            }
        }
        let tiles = self.stack_range_in(r.start..g, None, area.intersect(self.keep_bounds()));
        let folder = &self.layers[g];
        let blend = if folder.props.pass_through { BlendMode::Normal } else { folder.props.blend };
        let mut layer = Layer::pixels(LayerProps { pass_through: false, blend, ..folder.props.clone() }, tiles);
        layer.depth = folder.depth;
        layer.mask = folder.mask.clone();
        let before = g;
        let mut actions = Vec::new();
        for j in (r.start..g).rev() {
            let child = self.layers.remove(j);
            actions.push(Action::Presence { index: j, layer: Some(Box::new(child)) });
        }
        let old = std::mem::replace(&mut self.layers[r.start], layer);
        actions.push(Action::Swap { index: r.start, layer: Box::new(old) });
        self.active = r.start;
        self.mask_target = false;
        self.push(Action::Group(actions), before);
        true
    }

    /// 合并所有可见的图层（规格 F-056）。隐藏的图层原样留着（还在原来的文件夹里）；合并之后空了的文件夹去掉。
    /// 结果是一个普通的像素图层，放在最外层、参与合并的最下面那一组的位置上。看得见的图层不足两个时什么也不做。
    pub fn merge_visible(&mut self) -> bool {
        let n = self.layers.len();
        let shown = |doc: &Document, i: usize| doc.layers[i].props.visible && doc.ancestors(i).into_iter().all(|j| doc.layers[j].props.visible);
        let picked: Vec<usize> = (0..n).filter(|&i| !self.layers[i].is_folder() && shown(self, i)).collect();
        if picked.len() < 2 {
            return false;
        }
        let merged = self.stack_range(0..n, None);
        // 放在哪里：最下面那个参与合并的图层所在的最外层那一组的下沿，这样不会把哪个文件夹从中间切开。
        let lowest = picked[0];
        let root = self.ancestors(lowest).last().copied().unwrap_or(lowest);
        let gap = self.subtree(root).start;
        // 要去掉的：参与合并的图层，加上去掉它们之后空了的、看得见的文件夹。
        let mut gone = vec![false; n];
        for &i in &picked {
            gone[i] = true;
        }
        for g in 0..n {
            if self.layers[g].is_folder() && shown(self, g) {
                let r = self.subtree(g);
                if (r.start..g).all(|j| gone[j]) {
                    gone[g] = true;
                }
            }
        }
        let name = self.layers[lowest].props.name.clone();
        let before = self.active;
        let mut actions = Vec::new();
        for i in (0..n).rev().filter(|&i| gone[i]) {
            let layer = self.layers.remove(i);
            actions.push(Action::Presence { index: i, layer: Some(Box::new(layer)) });
        }
        let at = (0..gap).filter(|&i| !gone[i]).count();
        self.layers.insert(at, Layer::pixels(LayerProps { name, ..Default::default() }, merged));
        actions.push(Action::Presence { index: at, layer: None });
        self.active = at;
        self.mask_target = false;
        self.push(Action::Group(actions), before);
        true
    }

    /// 拼合（规格 F-056）：所有可见的图层连同背景色压成一个不透明的图层，隐藏的图层和文件夹都去掉。
    pub fn flatten(&mut self) {
        let base = [to15(self.background[0]), to15(self.background[1]), to15(self.background[2]), ONE];
        let merged = self.stack_range(0..self.layers.len(), Some(base));
        let name = self.layers[0].props.name.clone();
        let before = self.active;
        let mut actions = Vec::new();
        for i in (0..self.layers.len()).rev() {
            let layer = self.layers.remove(i);
            actions.push(Action::Presence { index: i, layer: Some(Box::new(layer)) });
        }
        self.layers.push(Layer::pixels(LayerProps { name, ..Default::default() }, merged));
        actions.push(Action::Presence { index: 0, layer: None });
        self.active = 0;
        self.mask_target = false;
        self.push(Action::Group(actions), before);
    }

    // ---- 蒙版 ----

    fn set_mask(&mut self, index: usize, mask: Option<Mask>) {
        let old = std::mem::replace(&mut self.layers[index].mask, mask);
        let active = self.active;
        self.push(Action::Mask { index, mask: old }, active);
    }

    /// 给当前图层加蒙版（规格 F-058）：有选区时选中的地方显示、其余隐藏；没有选区时全部显示。
    /// 加完之后编辑的是蒙版。已经有蒙版、或者图层锁定时什么也不做。
    pub fn add_mask(&mut self) -> bool {
        let i = self.active;
        if self.layers[i].mask.is_some() || self.is_locked(i) {
            return false;
        }
        let mask = match &self.selection {
            Some(sel) => {
                let mut m = Mask { tiles: GrayMap::default(), default: 0, disabled: false };
                for tc in tiles_in(sel.bounds()) {
                    if let Some(t) = sel.tile(tc).filter(|t| t.iter().any(|&v| v != 0)) {
                        m.tiles.replace(tc, Some(Arc::new(t.iter().map(|&v| v.min(ONE as u16)).collect())));
                    }
                }
                m
            }
            None => Mask::white(),
        };
        self.set_mask(i, Some(mask));
        self.mask_target = true;
        true
    }

    /// 删掉当前图层的蒙版。
    pub fn remove_mask(&mut self) -> bool {
        let i = self.active;
        if self.layers[i].mask.is_none() || self.is_locked(i) {
            return false;
        }
        self.set_mask(i, None);
        self.mask_target = false;
        true
    }

    /// 停用或者重新启用当前图层的蒙版。
    pub fn toggle_mask(&mut self) -> bool {
        let i = self.active;
        if self.is_locked(i) {
            return false;
        }
        let Some(mut m) = self.layers[i].mask.clone() else { return false };
        m.disabled = !m.disabled;
        self.set_mask(i, Some(m));
        true
    }

    /// 应用蒙版：乘进像素，再删掉蒙版。只对像素图层；停用的蒙版直接删掉。
    pub fn apply_mask(&mut self) -> bool {
        let i = self.active;
        let layer = &self.layers[i];
        let Some(mask) = layer.mask.as_ref() else { return false };
        if !layer.is_pixels() || self.is_locked(i) {
            return false;
        }
        let mut new = layer.clone();
        new.mask = None;
        if !mask.disabled {
            let mut tiles = TileMap::default();
            for tc in layer.tiles.coords() {
                let t = layer.tiles.get(tc).unwrap();
                let m = mask.tile(tc);
                let px: Vec<u16> = t.0.iter().enumerate().map(|(k, &v)| mul15(v as u32, m.map_or(mask.default, |m| m[k / 4]) as u32) as u16).collect();
                if px.chunks(4).any(|p| p[3] != 0) {
                    tiles.replace(tc, Some(Arc::new(Tile(px))));
                }
            }
            new.tiles = tiles;
        }
        let old = std::mem::replace(&mut self.layers[i], new);
        let active = self.active;
        self.push(Action::Swap { index: i, layer: Box::new(old) }, active);
        self.mask_target = false;
        true
    }

    // ---- 填充图层 ----

    /// 把当前的纯色填充图层改成 `color`（规格 F-061）。
    pub fn set_fill_color(&mut self, color: [u8; 3]) -> bool {
        let i = self.active;
        let Kind::Fill(Fill { paint: Paint::Solid(old), .. }) = &self.layers[i].kind else { return false };
        if *old == color || self.is_locked(i) {
            return false;
        }
        let mut new = self.layers[i].clone();
        new.kind = Kind::Fill(Fill { paint: Paint::Solid(color), psd: None });
        let old = std::mem::replace(&mut self.layers[i], new);
        let active = self.active;
        self.push(Action::Swap { index: i, layer: Box::new(old) }, active);
        true
    }

    /// 把当前的填充图层栅格化成普通的像素图层，蒙版保留。
    pub fn rasterize(&mut self) -> bool {
        let i = self.active;
        let Kind::Fill(fill) = &self.layers[i].kind else { return false };
        if self.is_locked(i) {
            return false;
        }
        // 还不会画的填充图层，栅格化之后就是它显示着的那些存着的像素。
        let mut tiles = if fill.paint == Paint::Other { self.layers[i].tiles.clone() } else { TileMap::default() };
        if let Paint::Solid(_) = fill.paint {
            let bounds = self.bounds();
            let px = fill_pixels(fill);
            for tc in tiles_in(bounds) {
                let (whole, clip) = (tile_rect(tc), tile_rect(tc).intersect(bounds));
                let t = tiles.get_mut(tc);
                for y in clip.y0..clip.y1 {
                    for x in clip.x0..clip.x1 {
                        let k = ((y - whole.y0) * TILE + (x - whole.x0)) as usize;
                        t.0[k * 4..k * 4 + 4].copy_from_slice(&px[k]);
                    }
                }
            }
        }
        let mut new = self.layers[i].clone();
        new.kind = Kind::Pixels;
        new.tiles = tiles;
        let old = std::mem::replace(&mut self.layers[i], new);
        let active = self.active;
        self.push(Action::Swap { index: i, layer: Box::new(old) }, active);
        true
    }
}

impl Document {
    /// 第 i 层（像素图层或填充图层）的像素，连同它自己和外面每一层文件夹的蒙版乘进去，填充图层画成像素，
    /// 只含画布里的部分。存成没有蒙版、没有填充图层的格式（ORA）时用。文件夹的蒙版这样乘进每个图层只是近似：
    /// 叠在一起的半透明部分和真的先合成再乘蒙版略有不同。
    pub fn baked_pixels(&self, i: usize) -> TileMap {
        let layer = &self.layers[i];
        let masks: Vec<&Mask> =
            std::iter::once(i).chain(self.ancestors(i)).filter_map(|j| self.layers[j].mask.as_ref().filter(|m| !m.disabled)).collect();
        let fill = match &layer.kind {
            Kind::Fill(f) if f.paint != Paint::Other => Some(fill_pixels(f)),
            _ => None,
        };
        if masks.is_empty() && fill.is_none() {
            return layer.tiles.clone();
        }
        // 填充图层铺满画布；像素图层连伸出画布的部分一起（实现方案 §3.15）。
        let area = if fill.is_some() { self.bounds() } else { self.keep_bounds() };
        let coords: Vec<TileCoord> =
            if fill.is_some() { tiles_in(area).collect() } else { layer.tiles.coords().filter(|&tc| !tile_rect(tc).intersect(area).is_empty()).collect() };
        let mut out = TileMap::default();
        for tc in coords {
            let src = layer.tiles.get(tc);
            if fill.is_none() && src.is_none() {
                continue;
            }
            let (whole, clip) = (tile_rect(tc), tile_rect(tc).intersect(area));
            let mut px = vec![0u16; N * 4];
            let mut any = false;
            for y in clip.y0..clip.y1 {
                for x in clip.x0..clip.x1 {
                    let k = ((y - whole.y0) * TILE + (x - whole.x0)) as usize;
                    let mut p = match (&fill, src) {
                        (Some(f), _) => f[k],
                        (None, Some(t)) => [t.0[k * 4], t.0[k * 4 + 1], t.0[k * 4 + 2], t.0[k * 4 + 3]],
                        (None, None) => [0; 4],
                    };
                    for m in &masks {
                        let v = m.tile(tc).map_or(m.default, |t| t[k]) as u32;
                        p = p.map(|c| mul15(c as u32, v) as u16);
                    }
                    any |= p[3] != 0;
                    px[k * 4..k * 4 + 4].copy_from_slice(&p);
                }
            }
            if any {
                out.replace(tc, Some(Arc::new(Tile(px))));
            }
        }
        out
    }
}

/// 一串图层（自下而上，`folder` 说哪些是文件夹）的深度合不合文件夹的规矩：从上往下看，
/// 最上面一行是 0，下一行最多比这一行深 1，而且只有这一行是文件夹时才能更深。
pub(super) fn valid_depths(folder: &[bool], depths: &[u16]) -> bool {
    let mut allowed = 0u16;
    for k in (0..depths.len()).rev() {
        if depths[k] > allowed {
            return false;
        }
        allowed = depths[k] + folder[k] as u16;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stabilizer::Sample;
    use crate::stroke::{BrushParams, Stroke};

    const RED: [u8; 3] = [220, 30, 30];
    const GRAY: [u8; 3] = [128, 128, 128];
    const BLUE: [u8; 3] = [20, 40, 230];
    const WHITE: [u8; 3] = [255, 255, 255];

    /// 一个铺着纯色方块的像素图层。
    fn solid(name: &str, depth: u16, color: [u8; 3], r: Rect) -> Layer {
        let mut tiles = TileMap::default();
        let px = [to15(color[0]) as u16, to15(color[1]) as u16, to15(color[2]) as u16, ONE as u16];
        for tc in tiles_in(r) {
            let t = tiles.get_mut(tc);
            for y in r.y0.max(tc.1 * TILE)..r.y1.min((tc.1 + 1) * TILE) {
                for x in r.x0.max(tc.0 * TILE)..r.x1.min((tc.0 + 1) * TILE) {
                    let i = ((y - tc.1 * TILE) * TILE + (x - tc.0 * TILE)) as usize;
                    t.0[i * 4..i * 4 + 4].copy_from_slice(&px);
                }
            }
        }
        let mut l = Layer::pixels(LayerProps { name: name.into(), ..Default::default() }, tiles);
        l.depth = depth;
        l
    }

    fn folder(name: &str, depth: u16, pass_through: bool) -> Layer {
        let mut f = Layer::folder(name);
        f.depth = depth;
        f.props.pass_through = pass_through;
        f
    }

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    fn doc_of(layers: Vec<Layer>) -> Document {
        Document::from_layers(200, 120, WHITE, layers)
    }

    /// 自下而上的图层名，每深一层前面加一个点。
    fn names(doc: &Document) -> Vec<String> {
        doc.layers().iter().map(|l| format!("{}{}", "·".repeat(l.depth as usize), l.props.name)).collect()
    }

    fn assert_valid(doc: &Document) {
        let folders: Vec<bool> = doc.layers().iter().map(|l| l.is_folder()).collect();
        let depths: Vec<u16> = doc.layers().iter().map(|l| l.depth).collect();
        assert!(valid_depths(&folders, &depths), "{:?}", names(doc));
    }

    /// 整张画布合成后的样子（每个像素的 RGB）。
    fn picture(doc: &Document) -> Vec<[u8; 3]> {
        (0..doc.height).flat_map(|y| (0..doc.width).map(move |x| (x, y))).map(|(x, y)| doc.pick(x, y).unwrap()).collect()
    }

    fn close(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(&p, q)| p.abs_diff(q) <= 1)
    }

    #[test]
    fn pass_through_folders_change_nothing_and_isolated_ones_isolate() {
        let mut gray = solid("multiply", 1, GRAY, rect(20, 20, 120, 100));
        gray.props.blend = BlendMode::Multiply;
        let base = solid("red", 0, RED, rect(0, 0, 200, 120));
        let mut flat = gray.clone();
        flat.depth = 0;
        let plain = doc_of(vec![base.clone(), flat]);
        let pass = doc_of(vec![base.clone(), gray.clone(), folder("f", 0, true)]);
        let isolated = doc_of(vec![base, gray, folder("f", 0, false)]);
        assert_eq!(picture(&pass), picture(&plain), "穿透的文件夹等于没有文件夹");
        // 隔离时"正片叠底"只和文件夹里面的透明底混合，结果就是它自己的灰色。
        assert!(close(isolated.pick(50, 50).unwrap(), GRAY), "{:?}", isolated.pick(50, 50));
        assert!(!close(plain.pick(50, 50).unwrap(), GRAY));
        assert_eq!(isolated.pick(5, 5), plain.pick(5, 5));
    }

    #[test]
    fn folder_visibility_and_opacity_apply_to_what_is_inside() {
        let mut doc = doc_of(vec![solid("red", 1, RED, rect(0, 0, 100, 120)), folder("f", 0, true), solid("blue", 0, BLUE, rect(150, 0, 200, 120))]);
        let p = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { visible: false, ..p.clone() });
        assert_eq!(doc.pick(50, 50), Some(WHITE), "隐藏的文件夹里的都不显示");
        assert_eq!(doc.pick(170, 50), Some(BLUE));
        for pass_through in [true, false] {
            doc.set_props_live(1, LayerProps { visible: true, opacity: 0.5, pass_through, ..p.clone() });
            let c = doc.pick(50, 50).unwrap();
            assert!(close(c, [238, 143, 143]), "{pass_through}: {c:?}");
        }
    }

    #[test]
    fn masks_hide_where_they_are_black() {
        let mut doc = doc_of(vec![solid("red", 0, RED, rect(0, 0, 200, 120))]);
        doc.set_selection(Some(Selection::rect(rect(0, 0, 100, 120))));
        assert!(doc.add_mask());
        assert!(doc.mask_target(), "加完蒙版之后编辑的是蒙版");
        assert_eq!(doc.pick(50, 50), Some(RED));
        assert_eq!(doc.pick(150, 50), Some(WHITE), "选区外被蒙版挡住");
        assert!(doc.toggle_mask());
        assert_eq!(doc.pick(150, 50), Some(RED), "停用的蒙版不起作用");
        doc.undo();
        assert_eq!(doc.pick(150, 50), Some(WHITE));
        // 应用蒙版：画面不变，蒙版没了，像素被乘掉。
        let before = picture(&doc);
        assert!(doc.apply_mask());
        assert!(doc.layers()[0].mask.is_none() && !doc.mask_target());
        assert_eq!(picture(&doc), before);
        doc.undo();
        assert!(doc.layers()[0].mask.is_some());
        assert!(doc.remove_mask());
        assert_eq!(doc.pick(150, 50), Some(RED));
        doc.undo();
        assert_eq!(doc.pick(150, 50), Some(WHITE));
        // 一个图层只有一个蒙版。
        assert!(!doc.add_mask());
    }

    fn stroke(doc: &mut Document, params: BrushParams, y: f32) {
        let (mut s, _) = Stroke::begin(params, 0, Sample { x: 10.0, y, p: 1.0 });
        s.add(Sample { x: 190.0, y, p: 1.0 });
        s.end();
        doc.commit_stroke(&s);
    }

    #[test]
    fn painting_on_the_mask_hides_and_the_eraser_reveals() {
        let mut doc = doc_of(vec![solid("red", 0, RED, rect(0, 0, 200, 120))]);
        doc.add_mask();
        let pixels = doc.layers()[0].tiles.clone();
        stroke(&mut doc, BrushParams { size: 20.0, color: [0, 0, 0], ..Default::default() }, 60.0);
        assert_eq!(doc.pick(100, 60), Some(WHITE), "黑色画在蒙版上就是挡住");
        assert_eq!(doc.pick(100, 20), Some(RED));
        assert!(doc.layers()[0].tiles.coords().all(|tc| Arc::ptr_eq(doc.layers()[0].tiles.get(tc).unwrap(), pixels.get(tc).unwrap())), "像素一点没动");
        stroke(&mut doc, BrushParams { size: 20.0, erase: true, ..Default::default() }, 60.0);
        assert_eq!(doc.pick(100, 60), Some(RED), "橡皮把蒙版擦回白色");
        doc.undo();
        assert_eq!(doc.pick(100, 60), Some(WHITE));
        // 灰色的笔画出半透明；清除是全部显示；填充前景色是前景色的明度。
        stroke(&mut doc, BrushParams { size: 20.0, color: GRAY, ..Default::default() }, 20.0);
        let half = doc.pick(100, 20).unwrap();
        assert!(half[1] > 100 && half[1] < 160, "{half:?}");
        doc.clear_active();
        assert_eq!(doc.pick(100, 60), Some(RED));
        doc.fill_active([0, 0, 0]);
        assert_eq!(doc.pick(100, 60), Some(WHITE));
        doc.set_mask_target(false);
        doc.clear_active();
        assert!(doc.layers()[0].tiles.is_empty(), "切回像素后清除的是像素");
    }

    #[test]
    fn moving_up_and_down_goes_in_and_out_of_folders() {
        let r = rect(0, 0, 10, 10);
        // 自下而上：bottom，文件夹 f（里面 a、b），top。
        let mut doc = doc_of(vec![solid("bottom", 0, RED, r), solid("a", 1, RED, r), solid("b", 1, RED, r), folder("f", 0, true), solid("top", 0, RED, r)]);
        let start = names(&doc);
        doc.set_active(0);
        // bottom 上移：上面是展开的文件夹，进到它里面的最下面。
        assert!(doc.move_active(1));
        assert_eq!(names(&doc), ["·bottom", "·a", "·b", "f", "top"]);
        // 再上移两次：和 a、b 对调。
        assert!(doc.move_active(1) && doc.move_active(1));
        assert_eq!(names(&doc), ["·a", "·b", "·bottom", "f", "top"]);
        // 已经是文件夹里最上面的：移出来，放到文件夹上面。
        assert!(doc.move_active(1));
        assert_eq!(names(&doc), ["·a", "·b", "f", "bottom", "top"]);
        assert_valid(&doc);
        // 下移：下面是展开的文件夹，进到它里面的最上面。
        assert!(doc.move_active(-1));
        assert_eq!(names(&doc), ["·a", "·b", "·bottom", "f", "top"]);
        // 折叠的文件夹整个跳过。
        doc.set_folder_open(3, false);
        doc.set_active(4);
        assert!(doc.move_active(-1));
        assert_eq!(names(&doc), ["top", "·a", "·b", "·bottom", "f"]);
        // 文件夹连同里面的一起挪。
        doc.set_active(4);
        assert!(doc.move_active(-1));
        assert_eq!(names(&doc), ["·a", "·b", "·bottom", "f", "top"]);
        assert_valid(&doc);
        // 文件夹里最下面的下移：移出来，到文件夹下面。
        doc.set_active(0);
        assert!(doc.move_active(-1));
        assert_eq!(names(&doc), ["a", "·b", "·bottom", "f", "top"]);
        assert_valid(&doc);
        // 全部撤销回到开始的样子。
        while doc.can_undo() {
            doc.undo();
        }
        assert_eq!(names(&doc), start);
        // 已经在最上面、或者在最下面而且不在文件夹里：不动。
        doc.set_active(4);
        assert!(!doc.move_active(1));
        doc.set_active(0);
        assert!(!doc.move_active(-1));
    }

    #[test]
    fn dragging_puts_layers_anywhere_but_not_into_themselves() {
        let r = rect(0, 0, 10, 10);
        let mut doc = doc_of(vec![solid("bottom", 0, RED, r), solid("a", 1, RED, r), folder("f", 0, true), solid("top", 0, RED, r)]);
        doc.set_active(3);
        // 拖进文件夹里面的最上面：文件夹那一行下面的空隙（序号 2），深一层。
        assert!(doc.move_active_to(2, 1));
        assert_eq!(names(&doc), ["bottom", "·a", "·top", "f"]);
        assert_eq!(doc.active(), 2);
        // 文件夹不能拖进自己里面。
        doc.set_active(3);
        assert!(!doc.move_active_to(2, 1));
        assert!(!doc.move_active_to(1, 1));
        // 文件夹连里面的一起拖到最下面。
        assert!(doc.move_active_to(0, 0));
        assert_eq!(names(&doc), ["·a", "·top", "f", "bottom"]);
        assert_valid(&doc);
        // 不合规矩的深度不接受：最上面一行必须是最外层。
        doc.set_active(0);
        assert!(!doc.move_active_to(4, 1));
        assert_eq!(names(&doc), ["·a", "·top", "f", "bottom"]);
        doc.undo();
        doc.undo();
        assert_eq!(names(&doc), ["bottom", "·a", "f", "top"]);
    }

    #[test]
    fn folder_operations_are_single_undo_steps() {
        let mut doc = doc_of(vec![solid("bottom", 0, RED, rect(0, 0, 100, 60)), solid("a", 0, BLUE, rect(50, 30, 150, 90))]);
        let pic = picture(&doc);
        let depth = doc.undo_depth();
        // Ctrl+G：a 放进新文件夹。
        doc.set_active(1);
        let f = doc.group_active();
        assert_eq!(names(&doc), ["bottom", "·a", "组 1"]);
        assert_eq!((f, doc.undo_depth()), (2, depth + 1));
        assert_eq!(picture(&doc), pic);
        // 文件夹展开时新建图层放进它里面的最上面；新建文件夹放在它上面。
        doc.add_layer();
        assert_eq!(names(&doc), ["bottom", "·a", "·图层 3", "组 1"]);
        doc.set_active(3);
        doc.add_folder();
        assert_eq!(names(&doc), ["bottom", "·a", "·图层 3", "组 1", "组 2"]);
        // 复制、删除文件夹连里面的一起。
        doc.set_active(3);
        doc.duplicate_active();
        assert_eq!(names(&doc), ["bottom", "·a", "·图层 3", "组 1", "·a", "·图层 3", "组 1 副本", "组 2"]);
        assert_eq!(doc.active(), 6);
        assert!(doc.remove_active());
        assert_eq!(names(&doc), ["bottom", "·a", "·图层 3", "组 1", "组 2"]);
        doc.undo();
        doc.undo();
        assert_eq!(names(&doc), ["bottom", "·a", "·图层 3", "组 1", "组 2"]);
        // 合并文件夹：画面不变。
        doc.set_active(3);
        assert!(doc.merge_down());
        assert_eq!(names(&doc), ["bottom", "组 1", "组 2"]);
        assert!(doc.layers()[1].is_pixels());
        assert_eq!(picture(&doc), pic);
        doc.undo();
        assert_eq!(names(&doc), ["bottom", "·a", "·图层 3", "组 1", "组 2"]);
        assert_valid(&doc);
        // 下面的兄弟是文件夹时不能向下合并；文件夹里最下面的也不能。
        doc.set_active(4);
        doc.add_layer();
        assert!(!doc.merge_down());
        doc.set_active(1);
        assert!(!doc.merge_down());
    }

    #[test]
    fn merging_visible_and_flattening_with_folders_keep_the_picture() {
        let mut hidden = solid("hidden", 1, GRAY, rect(0, 0, 200, 120));
        hidden.props.visible = false;
        let layers = vec![solid("bottom", 0, RED, rect(0, 0, 100, 60)), hidden, solid("a", 1, BLUE, rect(50, 30, 150, 90)), folder("f", 0, false), solid("top", 0, GRAY, rect(120, 0, 180, 40))];
        let mut doc = doc_of(layers);
        let pic = picture(&doc);
        assert!(doc.merge_visible());
        assert_eq!(picture(&doc), pic);
        // 隐藏的那一层还在原来的文件夹里。
        assert_eq!(names(&doc), ["bottom", "·hidden", "f"]);
        assert_valid(&doc);
        doc.undo();
        assert_eq!(names(&doc), ["bottom", "·hidden", "·a", "f", "top"]);
        doc.flatten();
        assert_eq!(names(&doc), ["bottom"]);
        assert_eq!(picture(&doc), pic);
    }

    #[test]
    fn clipping_inside_a_folder_uses_the_sibling_below() {
        let mut clipped = solid("clipped", 1, BLUE, rect(0, 0, 200, 120));
        clipped.props.clip = true;
        let doc = doc_of(vec![solid("outside", 0, RED, rect(0, 0, 200, 120)), solid("base", 1, GRAY, rect(0, 0, 50, 120)), clipped, folder("f", 0, true)]);
        assert_eq!(doc.pick(20, 20), Some(BLUE));
        assert_eq!(doc.pick(100, 20), Some(RED), "底只有左边一条，外面的红色不算底");
    }

    #[test]
    fn solid_fill_layers_cover_the_canvas_through_their_mask() {
        let mut fill = Layer::pixels(LayerProps { name: "颜色填充 1".into(), ..Default::default() }, TileMap::default());
        fill.kind = Kind::Fill(Fill { paint: Paint::Solid(BLUE), psd: Some((*b"SoCo", vec![1, 2, 3])) });
        let mut doc = doc_of(vec![solid("red", 0, RED, rect(0, 0, 100, 120)), fill]);
        assert_eq!(doc.pick(150, 50), Some(BLUE));
        assert!(!doc.can_edit_active(), "填充图层本身不能画");
        doc.set_selection(Some(Selection::rect(rect(0, 0, 100, 60))));
        doc.add_mask();
        doc.set_selection(None);
        assert!(doc.can_edit_active(), "它的蒙版可以画");
        assert_eq!((doc.pick(50, 30), doc.pick(50, 90)), (Some(BLUE), Some(RED)));
        assert!(doc.set_fill_color(GRAY));
        assert_eq!(doc.pick(50, 30), Some(GRAY));
        let Kind::Fill(f) = &doc.layers()[1].kind else { panic!("还应当是填充图层") };
        assert!(f.psd.is_none(), "改过颜色就不再原样写回原来的描述符");
        let pic = picture(&doc);
        assert!(doc.rasterize());
        assert!(doc.layers()[1].is_pixels() && doc.layers()[1].mask.is_some());
        assert_eq!(picture(&doc), pic);
        doc.undo();
        doc.undo();
        assert_eq!(doc.pick(50, 30), Some(BLUE));
    }

    #[test]
    fn what_can_be_painted() {
        let mut doc = doc_of(vec![solid("a", 1, RED, rect(0, 0, 10, 10)), folder("f", 0, true)]);
        doc.set_active(1);
        assert!(!doc.can_edit_active() && doc.active_open(), "文件夹本身不能画，但可以移动");
        doc.set_active(0);
        assert!(doc.can_edit_active());
        let p = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { visible: false, ..p.clone() });
        assert!(!doc.can_edit_active() && !doc.active_open(), "隐藏的文件夹里的不能画");
        doc.set_props_live(1, LayerProps { locked: true, ..p });
        assert!(!doc.can_edit_active(), "锁定的文件夹里的不能画");
        doc.set_mask_target(true);
        assert!(!doc.mask_target(), "没有蒙版时编辑的总是像素");
    }

    #[test]
    fn masks_survive_the_trip_through_ink() {
        for default in [0u16, ONE as u16] {
            let mut m = Mask { tiles: GrayMap::default(), default, disabled: true };
            let t = m.tiles.get_mut((1, 0), default);
            t[0] = 0;
            t[1] = 12345;
            t[2] = ONE as u16;
            let ink = m.to_ink();
            let back = m.with_ink(&ink);
            assert_eq!((back.default, back.disabled), (default, true));
            assert_eq!(back.tile((1, 0)).unwrap(), m.tile((1, 0)).unwrap());
            assert!(back.tile((0, 0)).is_none());
            // 默认值的地方墨是 0，也就是透明，移开以后露出来的还是默认值。
            assert_eq!(ink.get((1, 0)).unwrap().0[3 * 4 + 3], 0);
        }
    }

    #[test]
    fn replacing_several_parts_is_one_undo_step() {
        let mut a = solid("a", 1, RED, rect(0, 0, 10, 10));
        a.mask = Some(Mask::white());
        let mut doc = doc_of(vec![a, solid("b", 1, BLUE, rect(0, 0, 10, 10)), folder("f", 0, false)]);
        let before = picture(&doc);
        let moved = solid("x", 0, RED, rect(50, 50, 60, 60)).tiles;
        let sel = Some(Selection::rect(rect(50, 50, 60, 60)));
        doc.replace_parts(vec![(0, Part::Pixels(moved.clone())), (0, Part::Mask(None)), (1, Part::Pixels(TileMap::default()))], Some(sel));
        assert_eq!(doc.pick(55, 55), Some(RED));
        assert!(doc.layers()[0].mask.is_none() && doc.selection().is_some());
        doc.undo();
        assert_eq!(picture(&doc), before);
        assert!(doc.layers()[0].mask.is_some() && doc.selection().is_none() && !doc.can_undo());
        doc.redo();
        assert_eq!(doc.pick(55, 55), Some(RED));
    }

    /// 图层很多时合成一个分块要多久（0.3 的验收"不卡"）：200 个铺满画布的半透明图层，
    /// 平放着、放进 20 个穿透文件夹、放进 20 个普通文件夹，三种各合成全部分块。
    /// `cargo test --release -p zhibi-core many_layers_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn many_layers_timing() {
        let (w, h) = (1024, 1024);
        let make = |folders: Option<bool>| {
            let mut layers = Vec::new();
            for g in 0..20 {
                for k in 0..10 {
                    let mut l = solid(&format!("{g}-{k}"), folders.is_some() as u16, [(g * 12) as u8, (k * 25) as u8, 90], rect(0, 0, w, h));
                    l.props.opacity = 0.5;
                    l.props.blend = if k % 3 == 0 { BlendMode::Multiply } else { BlendMode::Normal };
                    layers.push(l);
                }
                if let Some(pass) = folders {
                    layers.push(folder(&format!("f{g}"), 0, pass));
                }
            }
            Document::from_layers(w, h, WHITE, layers)
        };
        let mut out = vec![0u32; N];
        for (name, doc) in [("平放 200 层", make(None)), ("20 个穿透文件夹", make(Some(true))), ("20 个普通文件夹", make(Some(false)))] {
            let tiles: Vec<TileCoord> = tiles_in(doc.bounds()).collect();
            let t = std::time::Instant::now();
            for &tc in &tiles {
                doc.composite_tile(None, tc, &mut out);
            }
            let per = t.elapsed().as_secs_f64() * 1000.0 / tiles.len() as f64;
            println!("{name}：{} 个分块，每块 {per:.3} 毫秒", tiles.len());
        }
    }

    /// 蒙版填成灰色（没有选区）：分块里是灰的，默认值仍是全黑或全白，所以移动、存盘都不会丢。
    #[test]
    fn a_grey_mask_fill_survives_moves_and_saving() {
        let mut doc = doc_of(vec![solid("a", 0, RED, rect(0, 0, 200, 120))]);
        doc.add_mask();
        doc.fill_active(GRAY);
        let m = doc.layers()[0].mask.clone().unwrap();
        assert!(m.default == 0 || m.default as u32 == ONE);
        let shown = doc.pick(5, 5).unwrap();
        assert!(shown[1] > 100 && shown[1] < 160, "半透明的红叠在白底上：{shown:?}");
        let back = m.with_ink(&m.to_ink());
        assert_eq!(back.tile((0, 0)), m.tile((0, 0)));
        let tmp = std::env::temp_dir().join(format!("zhibi-grey-mask-{}.zhibi", std::process::id()));
        crate::format::save(&doc, &tmp).unwrap();
        let loaded = crate::format::load(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(loaded.pick(5, 5), Some(shown));
    }

    /// 撤销、重做之后，编辑对象不会停在一个没有蒙版的图层的"蒙版"上。
    #[test]
    fn undo_puts_the_mask_target_back_to_pixels() {
        let mut doc = doc_of(vec![solid("a", 0, RED, rect(0, 0, 10, 10)), solid("b", 0, BLUE, rect(0, 0, 10, 10))]);
        doc.add_mask();
        assert!(doc.mask_target());
        doc.undo();
        assert!(!doc.mask_target() && doc.can_edit_active());
        doc.redo();
        assert!(!doc.mask_target());
        // 撤销换了当前图层：编辑对象回到像素。
        doc.set_mask_target(true);
        doc.set_active(0);
        doc.add_mask();
        doc.set_active(1);
        doc.set_mask_target(true);
        doc.undo();
        assert_eq!(doc.active(), 0);
        assert!(!doc.mask_target());
    }

    /// 组锁定时里面的东西都改不了：清除、填充、加蒙版、整体替换都不做。
    #[test]
    fn a_locked_group_locks_what_is_inside() {
        let mut doc = doc_of(vec![solid("a", 1, RED, rect(0, 0, 10, 10)), folder("f", 0, true)]);
        let p = doc.layers()[1].props.clone();
        doc.set_props_live(1, LayerProps { locked: true, ..p });
        doc.set_active(0);
        assert!(doc.is_locked(0));
        assert!(doc.clear_active().is_empty() && doc.fill_active(BLUE).is_empty());
        assert!(!doc.add_mask());
        assert!(doc.replace_parts(vec![(0, Part::Pixels(TileMap::default()))], None).is_empty());
        assert_eq!(doc.pick(5, 5), Some(RED));
        assert!(!doc.can_undo());
    }

    #[test]
    fn broken_depths_are_repaired_when_loading() {
        // 深度乱写的（自下而上）：普通图层下面更深、一下子深好几层、最上面不是 0。
        let doc = doc_of(vec![solid("a", 3, RED, rect(0, 0, 10, 10)), solid("b", 2, RED, rect(0, 0, 10, 10)), folder("f", 5, true)]);
        assert_eq!(names(&doc), ["·a", "·b", "f"]);
        assert_valid(&doc);
    }

    /// 导入为新图层（规格 F-148）：加图层和放像素并成一步；取消时撤掉而且不留重做；中间夹了别的步骤就不并。
    #[test]
    fn an_imported_layer_is_one_step_or_nothing() {
        let mut doc = doc_of(vec![solid("底", 0, RED, rect(0, 0, 200, 120)), folder("组", 0, false)]);
        doc.set_folder_open(1, true);
        let before = (names(&doc), doc.active(), doc.undo_depth());
        // 当前是展开的文件夹：新图层放进它里面。
        let index = doc.add_layer_with(Layer::new("图"));
        let step = doc.last_step().unwrap();
        assert_eq!((index, names(&doc)), (1, vec!["底".to_string(), "·图".into(), "组".into()]));
        doc.replace_parts(vec![(index, Part::Pixels(solid("x", 0, BLUE, rect(10, 10, 50, 50)).tiles))], None);
        assert!(doc.join_last_two(step));
        assert_eq!(doc.undo_depth(), before.2 + 1);
        doc.undo();
        assert_eq!((names(&doc), doc.active(), doc.undo_depth()), before);
        doc.redo();
        assert_eq!(doc.layers()[1].tiles.len(), 1);
        assert_eq!(doc.active(), 1);
        doc.undo();
        // 取消：撤掉加的那一步，重做里也没有它。
        let index = doc.add_layer_with(Layer::new("图"));
        let step = doc.last_step().unwrap();
        assert!(doc.forget_last(step));
        assert_eq!((names(&doc), doc.active(), doc.undo_depth(), doc.can_redo()), (before.0.clone(), before.1, before.2, false));
        assert!(!doc.is_modified());
        // 中间夹了一步改属性：不并，也不能直接撤掉。
        doc.add_layer_with(Layer::new("图"));
        let step = doc.last_step().unwrap();
        let p = doc.layers()[index].props.clone();
        doc.set_props_live(index, LayerProps { opacity: 0.5, ..p.clone() });
        doc.commit_props(index, p);
        assert!(!doc.forget_last(step));
        doc.replace_parts(vec![(index, Part::Pixels(TileMap::default()))], None);
        assert!(!doc.join_last_two(step));
        assert_valid(&doc);
    }
}
