// SPDX-License-Identifier: GPL-3.0-or-later
//! 移动和自由变换的状态与几何（规格 F-080、F-081、F-082）。
//!
//! 变换分三层（实现方案 §3.8），从里到外：网格上各点的偏移、四个角的偏移、外框的位置
//!（中心、两个方向的缩放、旋转角）。前两层记在外框自己的坐标里（变换前的像素），
//! 所以移动、缩放、旋转外框时，扭曲和弯曲跟着走。拖手柄只改这些数；
//! 像素每次都从进入变换时存下的原图重新算，所以来回拖多少次都不会越拖越糊。

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use zhibi_core::document::Document;
use zhibi_core::selection::Selection;
use zhibi_core::tile::TileMap;
use zhibi_core::transform::{self, Affine, Mapping, Quality};
use zhibi_core::warp::{self, Mesh, Perspective, Projective};
use zhibi_core::Rect;

/// 网格每边几个点。
pub const MESH: usize = 4;
const POINTS: usize = MESH * MESH;

/// 面板上的三种方式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 拖角缩放、拖边拉伸；按住 Ctrl 时和"扭曲"一样
    Free,
    /// 拖角只动这一个角，拖边是斜切
    Distort,
    /// 拖网格上的点
    Mesh,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Handle {
    /// 框内：移动
    Move,
    /// 框外：旋转
    Rotate,
    /// 拖的是哪条边或哪个角：-1、0、1 分别是左（上）、中间、右（下）
    Scale(i8, i8),
    /// 扭曲：只动这一个角（0 到 3，顺序同 `Xform::corners`）
    Corner(usize),
    /// 斜切：这条边顺着自己滑动。两个数的含义同 `Scale`，其中一个是 0
    Skew(i8, i8),
    /// 网格上的一点（0 到 15，逐行）
    Point(usize),
}

/// 外框的位置。
#[derive(Clone, Copy)]
struct Params {
    center: (f32, f32),
    scale: (f32, f32),
    angle: f32,
}

/// 按下手柄那一刻的样子：拖动都相对它来算。
#[derive(Clone, Copy)]
struct Grab {
    handle: Handle,
    params: Params,
    warp: [(f32, f32); 4],
    mesh: [(f32, f32); POINTS],
    /// 指针按下的位置
    from: (f32, f32),
    /// 按下时指针离手柄差多少；扣掉它，手柄才不会跳一下
    offset: (f32, f32),
}

pub struct Xform {
    pub layer: usize,
    /// 进入变换时这一层的像素
    pub orig: TileMap,
    /// 进入变换时的选区；有的话只变换选中的部分，选区也跟着走
    pub sel: Option<Selection>,
    /// 变换前内容的外框（画布坐标）
    rect: Rect,
    /// 外框中心现在的位置
    pub center: (f32, f32),
    /// 沿外框自己的横、纵方向的缩放，负数表示翻了面
    pub scale: (f32, f32),
    /// 弧度，正值在屏幕上为顺时针
    pub angle: f32,
    /// 面板上选的方式
    pub mode: Mode,
    /// 四个角的偏移（外框自己的坐标里，变换前的像素），顺序同 `corners`
    warp: [(f32, f32); 4],
    /// 网格上各点的偏移，同样在外框自己的坐标里
    mesh: [(f32, f32); POINTS],
    /// 参数变了，预览像素还没跟上
    pub stale: bool,
    /// 拖一下就确认（移动工具），不显示外框和手柄
    pub quick: bool,
    grab: Option<Grab>,
}

/// 外框四个角在 (u, v) 参数里的位置，顺着一圈排。
const CORNERS: [(i8, i8); 4] = [(-1, -1), (1, -1), (1, 1), (-1, 1)];

impl Xform {
    /// 对当前图层（有选区时是选中的部分）开始变换。没有可变换的内容时返回 None。
    pub fn begin(doc: &Document, quick: bool) -> Option<Xform> {
        let layer = doc.active();
        let orig = doc.layers()[layer].tiles.clone();
        let sel = doc.selection().cloned();
        let content = transform::content_bounds(&orig, doc.bounds());
        let rect = match &sel {
            Some(s) if content.intersect(s.bounds()).is_empty() => Rect::EMPTY,
            Some(s) => s.bounds().intersect(doc.bounds()),
            None => content,
        };
        if rect.is_empty() {
            return None;
        }
        Some(Xform::over(layer, orig, sel, rect, quick))
    }

    fn over(layer: usize, orig: TileMap, sel: Option<Selection>, rect: Rect, quick: bool) -> Xform {
        let mut x = Xform {
            layer,
            orig,
            sel,
            rect,
            center: (0.0, 0.0),
            scale: (1.0, 1.0),
            angle: 0.0,
            mode: Mode::Free,
            warp: [(0.0, 0.0); 4],
            mesh: [(0.0, 0.0); POINTS],
            stale: false,
            quick,
            grab: None,
        };
        x.center = x.origin();
        x
    }

    /// 外框变换前的中心。
    fn origin(&self) -> (f32, f32) {
        ((self.rect.x0 + self.rect.x1) as f32 / 2.0, (self.rect.y0 + self.rect.y1) as f32 / 2.0)
    }

    /// 外框变换前的半宽和半高。
    fn half(&self) -> (f32, f32) {
        ((self.rect.x1 - self.rect.x0) as f32 / 2.0, (self.rect.y1 - self.rect.y0) as f32 / 2.0)
    }

    /// 外框的位置（最外面那一层）：缩放、旋转、平移。
    pub fn matrix(&self) -> Affine {
        frame(&self.params(), self.origin())
    }

    pub fn is_identity(&self) -> bool {
        let c = self.origin();
        let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
        near(self.center.0, c.0) && near(self.center.1, c.1) && near(self.scale.0, 1.0) && near(self.scale.1, 1.0) && near(self.angle, 0.0) && !self.warped() && !self.bent()
    }

    /// 四个角动过（扭曲或斜切）。
    pub fn warped(&self) -> bool {
        self.warp.iter().any(|w| w.0 != 0.0 || w.1 != 0.0)
    }

    /// 网格弯过。
    pub fn bent(&self) -> bool {
        self.mesh.iter().any(|w| w.0 != 0.0 || w.1 != 0.0)
    }

    /// 外框在 (u, v)（0 到 1）处的一点落在画布上的哪里：先过网格和四个角，再过外框的位置。
    fn place(&self, u: f32, v: f32) -> (f32, f32) {
        place(&self.params(), self.rect, &self.warp, &self.mesh, u, v)
    }

    /// 手柄在画布上的位置。`hx`、`hy` 取 -1、0、1。
    pub fn handle(&self, hx: i8, hy: i8) -> (f32, f32) {
        self.place((hx as f32 + 1.0) / 2.0, (hy as f32 + 1.0) / 2.0)
    }

    /// 外框的四个角（画布坐标），顺着一圈排。
    pub fn corners(&self) -> [(f32, f32); 4] {
        CORNERS.map(|(hx, hy)| self.handle(hx, hy))
    }

    /// 网格上第 k 个点（逐行）在画布上的位置。
    pub fn mesh_point(&self, k: usize) -> (f32, f32) {
        let n = (MESH - 1) as f32;
        self.place((k % MESH) as f32 / n, (k / MESH) as f32 / n)
    }

    /// 外框的边线：网格弯过之后四条边也是弯的，所以每条边分成几段。
    pub fn outline(&self) -> Vec<(f32, f32)> {
        const STEPS: usize = 16;
        let mut line = Vec::with_capacity(4 * STEPS);
        for (a, b) in [((0.0, 0.0), (1.0, 0.0)), ((1.0, 0.0), (1.0, 1.0)), ((1.0, 1.0), (0.0, 1.0)), ((0.0, 1.0), (0.0, 0.0))] {
            for s in 0..STEPS {
                let t = s as f32 / STEPS as f32;
                line.push(self.place(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
            }
        }
        line
    }

    /// 网格里面的线（不含外框），每条一串点。
    pub fn mesh_lines(&self) -> Vec<Vec<(f32, f32)>> {
        const STEPS: usize = 24;
        let n = (MESH - 1) as f32;
        let mut lines = Vec::new();
        for k in 1..MESH - 1 {
            let c = k as f32 / n;
            lines.push((0..=STEPS).map(|s| self.place(s as f32 / STEPS as f32, c)).collect());
            lines.push((0..=STEPS).map(|s| self.place(c, s as f32 / STEPS as f32)).collect());
        }
        lines
    }

    fn params(&self) -> Params {
        Params { center: self.center, scale: self.scale, angle: self.angle }
    }

    /// 屏幕上的一点落在哪个手柄上。`to_screen` 把画布坐标换成屏幕坐标，`reach` 是手柄的判定半径。
    /// `distort` 是按着 Ctrl：自由方式下临时当扭曲用。
    pub fn hit(&self, p: (f32, f32), to_screen: impl Fn((f32, f32)) -> (f32, f32), reach: f32, distort: bool) -> Handle {
        let near = |c: (f32, f32)| {
            let h = to_screen(c);
            (h.0 - p.0).hypot(h.1 - p.1) <= reach
        };
        if self.mode == Mode::Mesh {
            if let Some(k) = (0..POINTS).find(|&k| near(self.mesh_point(k))) {
                return Handle::Point(k);
            }
        } else {
            let distort = distort || self.mode == Mode::Distort;
            // 角排在边的前面：外框很小时几个手柄会叠在一起，这时优先拿到角。
            const ORDER: [(i8, i8); 8] = [(-1, -1), (1, -1), (1, 1), (-1, 1), (0, -1), (1, 0), (0, 1), (-1, 0)];
            for (i, (hx, hy)) in ORDER.into_iter().enumerate() {
                if near(self.handle(hx, hy)) {
                    return match (distort, i < 4) {
                        (false, _) => Handle::Scale(hx, hy),
                        (true, true) => Handle::Corner(i),
                        (true, false) => Handle::Skew(hx, hy),
                    };
                }
            }
        }
        // 框内还是框外：从这一点往右引一条线，和边线交了奇数次就在里面。网格弯过时框可能是凹的，所以不用"在每条边的同一侧"。
        let q: Vec<(f32, f32)> = self.outline().into_iter().map(&to_screen).collect();
        let mut inside = false;
        for i in 0..q.len() {
            let (a, b) = (q[i], q[(i + 1) % q.len()]);
            if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                inside = !inside;
            }
        }
        if inside {
            Handle::Move
        } else {
            Handle::Rotate
        }
    }

    /// 按下手柄：记住这一刻的状态，之后的拖动都相对它来算。
    pub fn grab(&mut self, handle: Handle, at: (f32, f32)) {
        let anchor = match handle {
            Handle::Scale(hx, hy) => {
                // 缩放算的是外框本身的角和边（不含扭曲），扣的也是离它的距离。
                let r = self.rect;
                let (u, v) = ((hx as f32 + 1.0) / 2.0, (hy as f32 + 1.0) / 2.0);
                let p = (r.x0 as f32 + u * (r.x1 - r.x0) as f32, r.y0 as f32 + v * (r.y1 - r.y0) as f32);
                self.matrix().apply(p.0, p.1)
            }
            Handle::Corner(i) => self.handle(CORNERS[i].0, CORNERS[i].1),
            Handle::Point(k) => self.mesh_point(k),
            Handle::Move | Handle::Rotate | Handle::Skew(..) => at,
        };
        self.grab = Some(Grab { handle, params: self.params(), warp: self.warp, mesh: self.mesh, from: at, offset: (at.0 - anchor.0, at.1 - anchor.1) });
    }

    /// 拖动按下的手柄到画布上的 `p`。`constrain` 是 Shift：角上解除等比，旋转时按 15° 一档。
    pub fn drag(&mut self, p: (f32, f32), constrain: bool) {
        let Some(g) = self.grab else { return };
        let (start, from) = (g.params, g.from);
        // 拖到的位置扣掉按下时离手柄的那一点距离。
        let target = (p.0 - g.offset.0, p.1 - g.offset.1);
        match g.handle {
            Handle::Move => {
                self.center = (start.center.0 + p.0 - from.0, start.center.1 + p.1 - from.1);
            }
            Handle::Rotate => {
                let dir = |q: (f32, f32)| (q.1 - start.center.1).atan2(q.0 - start.center.0);
                let mut angle = start.angle + dir(p) - dir(from);
                if constrain {
                    let step = 15f32.to_radians();
                    angle = (angle / step).round() * step;
                }
                self.angle = wrap(angle);
            }
            Handle::Scale(hx, hy) => {
                let (hx, hy) = (hx as f32, hy as f32);
                let (w0, h0) = self.half();
                let (sin, cos) = start.angle.sin_cos();
                // 外框自己的横、纵方向
                let (u, v) = ((cos, sin), (-sin, cos));
                let (a0, b0) = (start.scale.0 * w0, start.scale.1 * h0);
                // 对面的边（或角）钉住不动。
                let anchor = corner(&start, (w0, h0), -hx, -hy);
                let d = (target.0 - anchor.0, target.1 - anchor.1);
                let (du, dv) = (d.0 * u.0 + d.1 * u.1, d.0 * v.0 + d.1 * v.1);
                let (mut a, mut b) = (a0, b0);
                if hx != 0.0 && hy != 0.0 && !constrain {
                    // 角上默认等比：把指针投影到对角线上。
                    let diag = (2.0 * hx * a0, 2.0 * hy * b0);
                    let k = (du * diag.0 + dv * diag.1) / (diag.0 * diag.0 + diag.1 * diag.1);
                    (a, b) = (k * a0, k * b0);
                } else {
                    if hx != 0.0 {
                        a = du * hx / 2.0;
                    }
                    if hy != 0.0 {
                        b = dv * hy / 2.0;
                    }
                }
                // 不让它缩成零：那样就再也拖不回来了。
                let keep = |v: f32| if v.abs() < 0.5 { 0.5f32.copysign(v) } else { v };
                let (a, b) = (keep(a), keep(b));
                self.scale = (a / w0, b / h0);
                self.center = (anchor.0 + hx * a * u.0 + hy * b * v.0, anchor.1 + hx * a * u.1 + hy * b * v.1);
            }
            Handle::Corner(i) => {
                // 画面上的角 = 外框位置 ∘（角的位置 + 那里的网格偏移），反过来解出角该在哪里。
                let Some(local) = self.matrix().inverse().map(|m| m.apply(target.0, target.1)) else { return };
                let rest = corner_of(self.rect, i);
                let k = mesh_corner(i);
                let mut warp = g.warp;
                warp[i] = (local.0 - g.mesh[k].0 - rest.0, local.1 - g.mesh[k].1 - rest.1);
                // 拖到会让外框凹进去或者交叉的地方，就停在上一个合法的位置。
                if warp::is_convex(&quad(self.rect, &warp)) {
                    self.warp = warp;
                }
            }
            Handle::Skew(hx, hy) => {
                // 这条边的两个角，沿着这条边按下时的方向一起滑动。
                let (i, j) = match (hx, hy) {
                    (0, -1) => (0, 1),
                    (1, 0) => (1, 2),
                    (0, 1) => (3, 2),
                    _ => (0, 3),
                };
                let q = quad(self.rect, &g.warp);
                let along = (q[j].0 - q[i].0, q[j].1 - q[i].1);
                let len = along.0.hypot(along.1);
                let Some(inv) = self.matrix().inverse() else { return };
                if len < 1e-6 {
                    return;
                }
                let (a, b) = (inv.apply(p.0, p.1), inv.apply(from.0, from.1));
                let dir = ((along.0 / len) as f32, (along.1 / len) as f32);
                let t = (a.0 - b.0) * dir.0 + (a.1 - b.1) * dir.1;
                let mut warp = g.warp;
                for c in [i, j] {
                    warp[c] = (warp[c].0 + t * dir.0, warp[c].1 + t * dir.1);
                }
                if warp::is_convex(&quad(self.rect, &warp)) {
                    self.warp = warp;
                }
            }
            Handle::Point(k) => {
                let Some(local) = self.matrix().inverse().map(|m| m.apply(target.0, target.1)) else { return };
                let n = (MESH - 1) as f32;
                let base = perspective_point(self.rect, &g.warp, (k % MESH) as f32 / n, (k / MESH) as f32 / n);
                self.mesh[k] = (local.0 - base.0, local.1 - base.1);
            }
        }
        self.snap();
        self.stale = true;
    }

    /// 在画布上左右（或上下）翻转。
    pub fn flip(&mut self, horizontal: bool) {
        // 先转再照镜子，等于反着转之后在外框自己的方向上翻面。扭曲和网格在外框自己的坐标里，跟着一起翻。
        self.angle = -self.angle;
        if horizontal {
            self.scale.0 = -self.scale.0;
        } else {
            self.scale.1 = -self.scale.1;
        }
        self.snap();
        self.stale = true;
    }

    /// 转 90°。
    pub fn turn(&mut self, clockwise: bool) {
        self.angle = wrap(self.angle + if clockwise { FRAC_PI_2 } else { -FRAC_PI_2 });
        self.snap();
        self.stale = true;
    }

    /// 回到进入变换时的样子：位置、扭曲、网格都清掉。
    pub fn reset(&mut self) {
        self.center = self.origin();
        self.scale = (1.0, 1.0);
        self.angle = 0.0;
        self.warp = [(0.0, 0.0); 4];
        self.mesh = [(0.0, 0.0); POINTS];
        self.stale = true;
    }

    /// 没有缩放、扭曲、弯曲，角度是 90° 的整数倍时，把位置对到整像素上：
    /// 这样结果是原像素原样搬过去，不经过插值，不会发虚。
    fn snap(&mut self) {
        let quarter = self.angle / FRAC_PI_2;
        let unit = |s: f32| (s.abs() - 1.0).abs() < 1e-4;
        if !unit(self.scale.0) || !unit(self.scale.1) || (quarter - quarter.round()).abs() > 1e-4 || self.warped() || self.bent() {
            return;
        }
        self.angle = wrap(quarter.round() * FRAC_PI_2);
        self.scale = (self.scale.0.signum(), self.scale.1.signum());
        let (w, h) = self.half();
        // 转了奇数个 90° 之后宽高互换。
        let (bw, bh) = if (quarter.round() as i32).rem_euclid(2) == 1 { (h, w) } else { (w, h) };
        self.center = ((self.center.0 - bw).round() + bw, (self.center.1 - bh).round() + bh);
    }

    /// 现在的变换落到像素上该走哪条路。
    fn mapping(&self) -> Option<Way> {
        let m = self.matrix();
        if !self.bent() {
            if parallelogram(&self.warp) {
                // 只做过斜切（或者什么都没做）：仍是仿射变换，走仿射那条路，整像素的移动照搬像素。
                let r = self.rect;
                let (w, h) = ((r.x1 - r.x0) as f32, (r.y1 - r.y0) as f32);
                let q = quad(r, &self.warp).map(|p| (p.0 as f32, p.1 as f32));
                let (ax, ay) = ((q[1].0 - q[0].0) / w, (q[1].1 - q[0].1) / w);
                let (bx, by) = ((q[3].0 - q[0].0) / h, (q[3].1 - q[0].1) / h);
                let (x0, y0) = (r.x0 as f32, r.y0 as f32);
                let shear = Affine { a: ax, b: ay, c: bx, d: by, e: q[0].0 - ax * x0 - bx * y0, f: q[0].1 - ay * x0 - by * y0 };
                let affine = if self.warped() { shear.then(&m) } else { m };
                return Some(Way::Affine(affine));
            }
            let h = Projective::rect_to_quad(self.rect, quad(self.rect, &self.warp))?;
            return Some(Way::Other(Box::new(Perspective::new(&h.then(&Projective::from_affine(&m)), self.rect)?)));
        }
        // 网格：源区域四周多留两个像素给插值核。
        let r = self.rect;
        let from = Rect { x0: r.x0 - 2, y0: r.y0 - 2, x1: r.x1 + 2, y1: r.y1 + 2 };
        let (w, h) = ((r.x1 - r.x0) as f64, (r.y1 - r.y0) as f64);
        let (params, warp, mesh) = (self.params(), self.warp, self.mesh);
        Some(Way::Other(Box::new(Mesh::new(from, |x, y| {
            let p = place(&params, r, &warp, &mesh, ((x - r.x0 as f64) / w) as f32, ((y - r.y0 as f64) / h) as f32);
            Some((p.0 as f64, p.1 as f64))
        }))))
    }

    pub fn render(&self, quality: Quality, clip: Rect) -> TileMap {
        match self.mapping() {
            Some(Way::Affine(m)) => transform::transform_tiles(&self.orig, self.sel.as_ref(), &m, quality, clip),
            Some(Way::Other(m)) => transform::map_tiles(&self.orig, self.sel.as_ref(), m.as_ref(), quality, clip),
            None => self.orig.clone(),
        }
    }

    /// 跟着内容一起变换后的选区。
    pub fn moved_selection(&self, clip: Rect) -> Option<Selection> {
        let sel = self.sel.as_ref()?;
        Some(match self.mapping() {
            Some(Way::Affine(m)) => transform::transform_selection(sel, &m, clip),
            Some(Way::Other(m)) => transform::map_selection(sel, m.as_ref(), clip),
            None => sel.clone(),
        })
    }
}

/// 变换落到像素上的两条路。
enum Way {
    Affine(Affine),
    Other(Box<dyn Mapping>),
}

/// 外框的位置：以外框原来的中心 `c` 缩放、旋转，再挪到现在的中心。
fn frame(p: &Params, c: (f32, f32)) -> Affine {
    Affine::scale_about(p.scale.0, p.scale.1, c.0, c.1).then(&Affine::rotate_about(p.angle, c.0, c.1)).then(&Affine::translate(p.center.0 - c.0, p.center.1 - c.1))
}

/// 外框本身（不算扭曲）的角或边中点在画布上的位置：`hx`、`hy` 是它在外框里的相对位置（-1 到 1）。
fn corner(p: &Params, half: (f32, f32), hx: f32, hy: f32) -> (f32, f32) {
    let (sin, cos) = p.angle.sin_cos();
    let (x, y) = (hx * half.0 * p.scale.0, hy * half.1 * p.scale.1);
    (p.center.0 + x * cos - y * sin, p.center.1 + x * sin + y * cos)
}

/// 矩形第 i 个角（顺序同 `CORNERS`）的位置。
fn corner_of(r: Rect, i: usize) -> (f32, f32) {
    let (hx, hy) = CORNERS[i];
    ((if hx < 0 { r.x0 } else { r.x1 }) as f32, (if hy < 0 { r.y0 } else { r.y1 }) as f32)
}

/// 外框第 i 个角在网格里是第几个点。
fn mesh_corner(i: usize) -> usize {
    let (hx, hy) = CORNERS[i];
    (if hy < 0 { 0 } else { MESH - 1 }) * MESH + if hx < 0 { 0 } else { MESH - 1 }
}

/// 扭曲后的四边形（外框自己的坐标）。
fn quad(r: Rect, warp: &[(f32, f32); 4]) -> [(f64, f64); 4] {
    std::array::from_fn(|i| {
        let c = corner_of(r, i);
        ((c.0 + warp[i].0) as f64, (c.1 + warp[i].1) as f64)
    })
}

/// 四个角的偏移是否仍构成平行四边形（左上 + 右下 = 右上 + 左下）。
fn parallelogram(warp: &[(f32, f32); 4]) -> bool {
    let (dx, dy) = (warp[0].0 + warp[2].0 - warp[1].0 - warp[3].0, warp[0].1 + warp[2].1 - warp[1].1 - warp[3].1);
    dx.abs() < 1e-3 && dy.abs() < 1e-3
}

/// 外框里 (u, v) 处的点只过扭曲（透视）之后在外框自己坐标里的位置。
fn perspective_point(r: Rect, warp: &[(f32, f32); 4], u: f32, v: f32) -> (f32, f32) {
    let (x, y) = (r.x0 as f64 + u as f64 * (r.x1 - r.x0) as f64, r.y0 as f64 + v as f64 * (r.y1 - r.y0) as f64);
    if warp.iter().all(|w| w.0 == 0.0 && w.1 == 0.0) {
        return (x as f32, y as f32);
    }
    let p = Projective::rect_to_quad(r, quad(r, warp)).and_then(|h| h.apply(x, y)).unwrap_or((x, y));
    (p.0 as f32, p.1 as f32)
}

/// 外框里 (u, v) 处的点落到画布上的位置：透视、加上网格偏移、再过外框的位置。
fn place(p: &Params, r: Rect, warp: &[(f32, f32); 4], mesh: &[(f32, f32); POINTS], u: f32, v: f32) -> (f32, f32) {
    let base = perspective_point(r, warp, u, v);
    let offsets: [(f64, f64); POINTS] = mesh.map(|o| (o.0 as f64, o.1 as f64));
    let bend = warp::lattice(&offsets, MESH, u as f64, v as f64);
    let c = ((r.x0 + r.x1) as f32 / 2.0, (r.y0 + r.y1) as f32 / 2.0);
    frame(p, c).apply(base.0 + bend.0 as f32, base.1 + bend.1 as f32)
}

fn wrap(a: f32) -> f32 {
    let a = a % TAU;
    if a > PI {
        a - TAU
    } else if a < -PI {
        a + TAU
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhibi_core::tile::TILE;
    use zhibi_core::ONE;

    fn sample() -> Xform {
        Xform::over(0, TileMap::default(), None, Rect { x0: 100, y0: 50, x1: 300, y1: 150 }, false)
    }

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-2 && (a.1 - b.1).abs() < 1e-2
    }

    /// 外框是 `r` 的变换，内容是 `ink` 里的不透明白色。
    fn with_content(r: Rect, ink: Rect) -> Xform {
        let mut tiles = TileMap::default();
        for y in ink.y0..ink.y1 {
            for x in ink.x0..ink.x1 {
                let t = tiles.get_mut((x.div_euclid(TILE), y.div_euclid(TILE)));
                let i = (y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize * 4;
                t.0[i..i + 4].copy_from_slice(&[ONE as u16; 4]);
            }
        }
        Xform::over(0, tiles, None, r, false)
    }

    fn alpha(tiles: &TileMap, x: i32, y: i32) -> u32 {
        let i = (y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize * 4 + 3;
        tiles.get((x.div_euclid(TILE), y.div_euclid(TILE))).map_or(0, |t| t.0[i] as u32)
    }

    #[test]
    fn handles_agree_with_the_matrix() {
        let mut x = sample();
        (x.center, x.scale, x.angle) = ((230.0, 80.0), (1.5, -0.5), 0.7);
        let m = x.matrix();
        let r = x.rect;
        let before = [(r.x0, r.y0), (r.x1, r.y0), (r.x1, r.y1), (r.x0, r.y1)];
        for (c, b) in x.corners().into_iter().zip(before) {
            assert!(close(c, m.apply(b.0 as f32, b.1 as f32)), "{c:?}");
        }
        assert!(!x.is_identity());
        x.reset();
        assert!(x.is_identity());
    }

    #[test]
    fn dragging_a_corner_keeps_the_opposite_corner_and_the_proportions() {
        let mut x = sample();
        x.grab(Handle::Scale(1, 1), (300.0, 150.0));
        x.drag((400.0, 170.0), false);
        assert!(close(x.handle(-1, -1), (100.0, 50.0)), "{:?}", x.handle(-1, -1));
        assert!((x.scale.0 - x.scale.1).abs() < 1e-4 && x.scale.0 > 1.0);
        // 按住 Shift 解除等比：手柄正好跟到指针上。
        x.drag((400.0, 170.0), true);
        assert!(close(x.handle(1, 1), (400.0, 170.0)));
        assert!(close(x.handle(-1, -1), (100.0, 50.0)));
    }

    #[test]
    fn dragging_a_side_stretches_one_direction_only() {
        let mut x = sample();
        x.grab(Handle::Scale(1, 0), (303.0, 100.0));
        x.drag((353.0, 130.0), false);
        // 按下时离手柄偏了 3 个像素，这 3 个像素不算进拉伸里。
        assert!(close(x.handle(1, 0), (350.0, 100.0)), "{:?}", x.handle(1, 0));
        assert!(close(x.handle(-1, 0), (100.0, 100.0)));
        assert!((x.scale.1 - 1.0).abs() < 1e-4);
        // 拖过对边就是翻面，不会卡在零上。
        x.drag((53.0, 100.0), false);
        assert!(x.scale.0 < 0.0);
        x.drag((103.0, 100.0), false);
        assert!(x.scale.0.abs() > 0.0);
    }

    #[test]
    fn rotation_and_moves_snap_to_whole_pixels_when_nothing_is_resampled() {
        let mut x = sample();
        x.grab(Handle::Move, (200.0, 100.0));
        x.drag((210.4, 97.7), false);
        assert!(close(x.center, (210.0, 98.0)), "{:?}", x.center);
        // 200×100 的框转 90° 后是 100×200，角点仍然落在整数上。
        x.turn(true);
        for c in x.corners() {
            assert!((c.0 - c.0.round()).abs() < 1e-3 && (c.1 - c.1.round()).abs() < 1e-3, "{c:?}");
        }
        // 宽高一奇一偶时中心要挪半个像素才对得上。
        let mut y = sample();
        y.rect.x1 = 301;
        y.center = y.origin();
        y.turn(false);
        for c in y.corners() {
            assert!((c.0 - c.0.round()).abs() < 1e-3 && (c.1 - c.1.round()).abs() < 1e-3, "{c:?}");
        }
        // 四次 90° 回到原样。
        for _ in 0..3 {
            x.turn(true);
        }
        assert!((x.angle).abs() < 1e-4);
    }

    #[test]
    fn rotating_with_shift_goes_in_steps() {
        let mut x = sample();
        x.grab(Handle::Rotate, (400.0, 100.0));
        x.drag((400.0, 160.0), true);
        let deg = x.angle.to_degrees();
        assert!((deg / 15.0 - (deg / 15.0).round()).abs() < 1e-3 && deg > 0.0, "{deg}");
    }

    #[test]
    fn flipping_twice_is_a_no_op_and_flips_in_canvas_directions() {
        let mut x = sample();
        x.angle = 0.5;
        let before = x.corners();
        x.flip(true);
        // 左右翻转后，原来的左上角到了右边：与原来的右上角关于中心竖线对称。
        let c = x.corners();
        assert!(close(c[0], (2.0 * x.center.0 - before[0].0, before[0].1)), "{:?}", c[0]);
        x.flip(true);
        for (a, b) in x.corners().into_iter().zip(before) {
            assert!(close(a, b));
        }
    }

    #[test]
    fn hit_testing_finds_handles_inside_and_outside() {
        let x = sample();
        let same = |p: (f32, f32)| p;
        assert_eq!(x.hit((302.0, 148.0), same, 6.0, false), Handle::Scale(1, 1));
        assert_eq!(x.hit((200.0, 52.0), same, 6.0, false), Handle::Scale(0, -1));
        assert_eq!(x.hit((180.0, 110.0), same, 6.0, false), Handle::Move);
        assert_eq!(x.hit((340.0, 110.0), same, 6.0, false), Handle::Rotate);
        // 按着 Ctrl，或者选了"扭曲"：角是扭曲，边是斜切。
        assert_eq!(x.hit((302.0, 148.0), same, 6.0, true), Handle::Corner(2));
        assert_eq!(x.hit((200.0, 52.0), same, 6.0, true), Handle::Skew(0, -1));
        let mut d = sample();
        d.mode = Mode::Distort;
        assert_eq!(d.hit((98.0, 52.0), same, 6.0, false), Handle::Corner(0));
        // 网格方式下只认网格上的点；左上第二行第二个点在 (100 + 200/3, 50 + 100/3)。
        d.mode = Mode::Mesh;
        assert_eq!(d.hit((166.0, 84.0), same, 6.0, false), Handle::Point(5));
        assert_eq!(d.hit((302.0, 148.0), same, 6.0, false), Handle::Point(15));
        assert_eq!(d.hit((200.0, 100.0), same, 6.0, false), Handle::Move);
    }

    #[test]
    fn a_distorted_corner_lands_under_the_pointer_and_rides_along() {
        let mut x = sample();
        // 按下时指针离角差 (2, -1)，这点差别在拖动中保持不变。
        x.grab(Handle::Corner(2), (302.0, 149.0));
        x.drag((342.0, 189.0), false);
        assert!(close(x.corners()[2], (340.0, 190.0)), "{:?}", x.corners());
        assert!(close(x.corners()[0], (100.0, 50.0)) && close(x.corners()[1], (300.0, 50.0)) && close(x.corners()[3], (100.0, 150.0)));
        assert!(x.warped() && !x.is_identity());
        // 拖进对角线里面会让外框凹进去：停在上一个位置。
        x.drag((150.0, 70.0), false);
        assert!(close(x.corners()[2], (340.0, 190.0)), "{:?}", x.corners());
        // 扭曲之后再整体移动、旋转，四个角一起走，形状不变。
        let before = x.corners();
        x.grab(Handle::Move, (200.0, 100.0));
        x.drag((230.0, 90.0), false);
        for (a, b) in x.corners().into_iter().zip(before) {
            assert!(close(a, (b.0 + 30.0, b.1 - 10.0)), "{a:?} {b:?}");
        }
        x.turn(true);
        let turned = x.corners();
        let d = |p: (f32, f32), q: (f32, f32)| (p.0 - q.0).hypot(p.1 - q.1);
        assert!((d(turned[0], turned[2]) - d(before[0], before[2])).abs() < 1e-2);
        x.reset();
        assert!(x.is_identity());
    }

    #[test]
    fn skewing_slides_an_edge_along_itself() {
        let mut x = sample();
        x.grab(Handle::Skew(0, -1), (200.0, 50.0));
        x.drag((240.0, 70.0), false);
        // 上边整体向右滑了 40，竖直方向的拖动不算；下边不动。
        let c = x.corners();
        assert!(close(c[0], (140.0, 50.0)) && close(c[1], (340.0, 50.0)), "{c:?}");
        assert!(close(c[2], (300.0, 150.0)) && close(c[3], (100.0, 150.0)), "{c:?}");
        // 只斜切过，仍是仿射变换。
        assert!(matches!(x.mapping(), Some(Way::Affine(_))));
        x.grab(Handle::Corner(0), (140.0, 50.0));
        x.drag((150.0, 40.0), false);
        assert!(matches!(x.mapping(), Some(Way::Other(_))));
    }

    #[test]
    fn skew_and_distortion_render_where_the_frame_says() {
        let r = Rect { x0: 40, y0: 40, x1: 140, y1: 120 };
        let mut x = with_content(r, r);
        let clip = Rect { x0: 0, y0: 0, x1: 260, y1: 220 };
        x.grab(Handle::Skew(0, 1), (90.0, 120.0));
        x.drag((130.0, 120.0), false);
        let out = x.render(Quality::Best, clip);
        // 下边滑到了 80..180，上边还在 40..140。
        assert_eq!(alpha(&out, 45, 42), ONE);
        assert_eq!(alpha(&out, 175, 117), ONE);
        assert_eq!(alpha(&out, 45, 117), 0);
        x.grab(Handle::Corner(1), (140.0, 40.0));
        x.drag((200.0, 20.0), false);
        let out = x.render(Quality::Fast, clip);
        assert_eq!(alpha(&out, 192, 25), ONE, "拖出去的角那里有内容");
        assert_eq!(alpha(&out, 150, 22), 0);
    }

    #[test]
    fn bending_moves_the_point_and_its_surroundings() {
        // 外框 60..180，里面一条横线正好穿过第二行的网格点（y = 60 + 120/3 = 100）。
        let mut x = with_content(Rect { x0: 60, y0: 60, x1: 180, y1: 180 }, Rect { x0: 60, y0: 99, x1: 180, y1: 101 });
        x.mode = Mode::Mesh;
        let p = x.mesh_point(5);
        assert!(close(p, (100.0, 100.0)), "{p:?}");
        x.grab(Handle::Point(5), (101.0, 100.0));
        x.drag((81.0, 70.0), false);
        assert!(close(x.mesh_point(5), (80.0, 70.0)), "{:?}", x.mesh_point(5));
        assert!(x.bent() && !x.warped());
        // 别的点不动，外框四个角也不动。
        assert!(close(x.mesh_point(10), (140.0, 140.0)) && close(x.corners()[0], (60.0, 60.0)));
        // 线跟着那个点走到了 (80, 70)，原来的位置空了。
        let out = x.render(Quality::Best, Rect { x0: 0, y0: 0, x1: 240, y1: 240 });
        // 线的中心线正好经过 (80, 70)，落在两行像素的交界上，所以看它周围 5×5 里的墨量。
        let around = |cx: i32, cy: i32| (-2..=2).flat_map(|dy| (-2..=2).map(move |dx| (dx, dy))).map(|(dx, dy)| alpha(&out, cx + dx, cy + dy)).sum::<u32>();
        assert!(around(80, 70) > 2 * ONE, "{}", around(80, 70));
        assert_eq!(around(100, 100), 0);
        // 线的两头钉在外框的边上没动（往里走一点就跟着弯了，所以只看最外面那一列）。
        let end = |x: i32| (97..103).map(|y| alpha(&out, x, y)).sum::<u32>();
        assert!(end(60) > ONE && end(179) > ONE, "{} {}", end(60), end(179));
        // 网格弯过之后整体旋转，网格点跟着转。
        x.grab(Handle::Rotate, (200.0, 120.0));
        x.drag((120.0, 200.0), false);
        let c = x.matrix().apply(80.0, 70.0);
        assert!(close(x.mesh_point(5), c), "{:?} {c:?}", x.mesh_point(5));
    }
}
