// SPDX-License-Identifier: GPL-3.0-or-later
//! 移动和自由变换的状态与几何（规格 F-080、F-081、F-082）。
//!
//! 变换用"中心、两个方向的缩放、旋转角"这几个数表示，拖手柄只改这几个数；
//! 像素每次都从进入变换时存下的原图重新算，所以来回拖多少次都不会越拖越糊。

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use zhibi_core::document::Document;
use zhibi_core::selection::Selection;
use zhibi_core::tile::TileMap;
use zhibi_core::transform::{self, Affine, Quality};
use zhibi_core::Rect;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Handle {
    /// 框内：移动
    Move,
    /// 框外：旋转
    Rotate,
    /// 拖的是哪条边或哪个角：-1、0、1 分别是左（上）、中间、右（下）
    Scale(i8, i8),
}

#[derive(Clone, Copy)]
struct Params {
    center: (f32, f32),
    scale: (f32, f32),
    angle: f32,
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
    /// 参数变了，预览像素还没跟上
    pub stale: bool,
    /// 拖一下就确认（移动工具），不显示外框和手柄
    pub quick: bool,
    /// 按下手柄那一刻的参数和指针位置
    grab: Option<(Params, (f32, f32))>,
}

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
        let mut x = Xform { layer, orig, sel, rect, center: (0.0, 0.0), scale: (1.0, 1.0), angle: 0.0, stale: false, quick, grab: None };
        x.center = x.origin();
        Some(x)
    }

    /// 外框变换前的中心。
    fn origin(&self) -> (f32, f32) {
        ((self.rect.x0 + self.rect.x1) as f32 / 2.0, (self.rect.y0 + self.rect.y1) as f32 / 2.0)
    }

    /// 外框变换前的半宽和半高。
    fn half(&self) -> (f32, f32) {
        ((self.rect.x1 - self.rect.x0) as f32 / 2.0, (self.rect.y1 - self.rect.y0) as f32 / 2.0)
    }

    pub fn matrix(&self) -> Affine {
        let c = self.origin();
        Affine::scale_about(self.scale.0, self.scale.1, c.0, c.1)
            .then(&Affine::rotate_about(self.angle, c.0, c.1))
            .then(&Affine::translate(self.center.0 - c.0, self.center.1 - c.1))
    }

    pub fn is_identity(&self) -> bool {
        let c = self.origin();
        let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
        near(self.center.0, c.0) && near(self.center.1, c.1) && near(self.scale.0, 1.0) && near(self.scale.1, 1.0) && near(self.angle, 0.0)
    }

    /// 手柄在画布上的位置。`hx`、`hy` 取 -1、0、1。
    pub fn handle(&self, hx: i8, hy: i8) -> (f32, f32) {
        place(&self.params(), self.half(), hx as f32, hy as f32)
    }

    /// 外框的四个角（画布坐标），顺着一圈排。
    pub fn corners(&self) -> [(f32, f32); 4] {
        [(-1, -1), (1, -1), (1, 1), (-1, 1)].map(|(hx, hy)| self.handle(hx, hy))
    }

    fn params(&self) -> Params {
        Params { center: self.center, scale: self.scale, angle: self.angle }
    }

    /// 屏幕上的一点落在哪个手柄上。`to_screen` 把画布坐标换成屏幕坐标，`reach` 是手柄的判定半径。
    pub fn hit(&self, p: (f32, f32), to_screen: impl Fn((f32, f32)) -> (f32, f32), reach: f32) -> Handle {
        // 角排在边的前面：外框很小时几个手柄会叠在一起，这时优先拿到角。
        const ORDER: [(i8, i8); 8] = [(-1, -1), (1, -1), (1, 1), (-1, 1), (0, -1), (1, 0), (0, 1), (-1, 0)];
        for (hx, hy) in ORDER {
            let h = to_screen(self.handle(hx, hy));
            if (h.0 - p.0).hypot(h.1 - p.1) <= reach {
                return Handle::Scale(hx, hy);
            }
        }
        let q = self.corners().map(&to_screen);
        // 四条边的同一侧就是在框内。翻转过的框绕向相反，所以两种符号都算。
        let side = |i: usize| {
            let (a, b) = (q[i], q[(i + 1) % 4]);
            (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
        };
        let s: [f32; 4] = [side(0), side(1), side(2), side(3)];
        if s.iter().all(|v| *v >= 0.0) || s.iter().all(|v| *v <= 0.0) {
            Handle::Move
        } else {
            Handle::Rotate
        }
    }

    /// 按下手柄：记住这一刻的状态，之后的拖动都相对它来算。
    pub fn grab(&mut self, at: (f32, f32)) {
        self.grab = Some((self.params(), at));
    }

    /// 拖动手柄到画布上的 `p`。`constrain` 是 Shift：角上解除等比，旋转时按 15° 一档。
    pub fn drag(&mut self, handle: Handle, p: (f32, f32), constrain: bool) {
        let Some((start, from)) = self.grab else { return };
        match handle {
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
                let anchor = place(&start, (w0, h0), -hx, -hy);
                // 按下时指针离手柄可能差几个像素，扣掉这段，手柄才不会跳一下。
                let grabbed = place(&start, (w0, h0), hx, hy);
                let d = (p.0 - (from.0 - grabbed.0) - anchor.0, p.1 - (from.1 - grabbed.1) - anchor.1);
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
        }
        self.snap();
        self.stale = true;
    }

    /// 在画布上左右（或上下）翻转。
    pub fn flip(&mut self, horizontal: bool) {
        // 先转再照镜子，等于反着转之后在外框自己的方向上翻面。
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

    pub fn reset(&mut self) {
        self.center = self.origin();
        self.scale = (1.0, 1.0);
        self.angle = 0.0;
        self.stale = true;
    }

    /// 没有缩放、角度是 90° 的整数倍时，把位置对到整像素上：
    /// 这样结果是原像素原样搬过去，不经过插值，不会发虚。
    fn snap(&mut self) {
        let quarter = self.angle / FRAC_PI_2;
        let unit = |s: f32| (s.abs() - 1.0).abs() < 1e-4;
        if !unit(self.scale.0) || !unit(self.scale.1) || (quarter - quarter.round()).abs() > 1e-4 {
            return;
        }
        self.angle = wrap(quarter.round() * FRAC_PI_2);
        self.scale = (self.scale.0.signum(), self.scale.1.signum());
        let (w, h) = self.half();
        // 转了奇数个 90° 之后宽高互换。
        let (bw, bh) = if (quarter.round() as i32).rem_euclid(2) == 1 { (h, w) } else { (w, h) };
        self.center = ((self.center.0 - bw).round() + bw, (self.center.1 - bh).round() + bh);
    }

    pub fn render(&self, quality: Quality, clip: Rect) -> TileMap {
        transform::transform_tiles(&self.orig, self.sel.as_ref(), &self.matrix(), quality, clip)
    }

    /// 跟着内容一起变换后的选区。
    pub fn moved_selection(&self, clip: Rect) -> Option<Selection> {
        self.sel.as_ref().map(|s| transform::transform_selection(s, &self.matrix(), clip))
    }
}

/// 外框上的一点在画布上的位置：`hx`、`hy` 是它在外框里的相对位置（-1 到 1）。
fn place(p: &Params, half: (f32, f32), hx: f32, hy: f32) -> (f32, f32) {
    let (sin, cos) = p.angle.sin_cos();
    let (x, y) = (hx * half.0 * p.scale.0, hy * half.1 * p.scale.1);
    (p.center.0 + x * cos - y * sin, p.center.1 + x * sin + y * cos)
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

    fn sample() -> Xform {
        Xform {
            layer: 0,
            orig: TileMap::default(),
            sel: None,
            rect: Rect { x0: 100, y0: 50, x1: 300, y1: 150 },
            center: (200.0, 100.0),
            scale: (1.0, 1.0),
            angle: 0.0,
            stale: false,
            quick: false,
            grab: None,
        }
    }

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-2 && (a.1 - b.1).abs() < 1e-2
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
        x.grab((300.0, 150.0));
        x.drag(Handle::Scale(1, 1), (400.0, 170.0), false);
        assert!(close(x.handle(-1, -1), (100.0, 50.0)), "{:?}", x.handle(-1, -1));
        assert!((x.scale.0 - x.scale.1).abs() < 1e-4 && x.scale.0 > 1.0);
        // 按住 Shift 解除等比：手柄正好跟到指针上。
        x.drag(Handle::Scale(1, 1), (400.0, 170.0), true);
        assert!(close(x.handle(1, 1), (400.0, 170.0)));
        assert!(close(x.handle(-1, -1), (100.0, 50.0)));
    }

    #[test]
    fn dragging_a_side_stretches_one_direction_only() {
        let mut x = sample();
        x.grab((303.0, 100.0));
        x.drag(Handle::Scale(1, 0), (353.0, 130.0), false);
        // 按下时离手柄偏了 3 个像素，这 3 个像素不算进拉伸里。
        assert!(close(x.handle(1, 0), (350.0, 100.0)), "{:?}", x.handle(1, 0));
        assert!(close(x.handle(-1, 0), (100.0, 100.0)));
        assert!((x.scale.1 - 1.0).abs() < 1e-4);
        // 拖过对边就是翻面，不会卡在零上。
        x.drag(Handle::Scale(1, 0), (53.0, 100.0), false);
        assert!(x.scale.0 < 0.0);
        x.drag(Handle::Scale(1, 0), (103.0, 100.0), false);
        assert!(x.scale.0.abs() > 0.0);
    }

    #[test]
    fn rotation_and_moves_snap_to_whole_pixels_when_nothing_is_resampled() {
        let mut x = sample();
        x.grab((200.0, 100.0));
        x.drag(Handle::Move, (210.4, 97.7), false);
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
        x.grab((400.0, 100.0));
        x.drag(Handle::Rotate, (400.0, 160.0), true);
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
        assert_eq!(x.hit((302.0, 148.0), same, 6.0), Handle::Scale(1, 1));
        assert_eq!(x.hit((200.0, 52.0), same, 6.0), Handle::Scale(0, -1));
        assert_eq!(x.hit((180.0, 110.0), same, 6.0), Handle::Move);
        assert_eq!(x.hit((340.0, 110.0), same, 6.0), Handle::Rotate);
    }
}
