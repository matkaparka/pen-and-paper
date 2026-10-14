// SPDX-License-Identifier: GPL-3.0-or-later
//! 画布视图：缩放、旋转、翻转、平移是一个仿射变换；显示时把屏幕像素逆变换回画布取色。
//!
//! 只重画被要求的屏幕矩形，所以一笔只会触发它碰到的那一小块重画。

use std::collections::HashMap;

use crate::document::Document;
use crate::selection::Selection;
use crate::stroke::Stroke;
use crate::tile::{tiles_in, TileCoord, TILE};
use crate::{Rect, ONE};

/// 画布以外的底色。
pub const PASTEBOARD: u32 = 0x00A8_A8A8;

/// 要重画的屏幕像素不少于这么多、或者要重新合成的分块不少于这么多时，才分给几个线程去做。
/// 一笔带来的重画远小于这个数，仍然在当前线程上立刻做完，不付开线程的代价。
const PARALLEL_PIXELS: i64 = 150_000;
const PARALLEL_TILES: usize = 64;

fn workers() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8)
}

#[derive(Clone, Copy, Debug)]
pub struct View {
    /// 1.0 为 100%
    pub zoom: f32,
    /// 弧度，正值为屏幕上顺时针
    pub angle: f32,
    /// 水平翻转视图（不改画布）
    pub flip: bool,
    /// 显示在窗口正中的画布坐标
    pub center: (f32, f32),
}

impl View {
    /// 画布到屏幕的线性部分：先翻转，再旋转，再缩放。
    fn matrix(&self) -> [f32; 4] {
        let (s, c) = self.angle.sin_cos();
        let f = if self.flip { -1.0 } else { 1.0 };
        [self.zoom * c * f, -self.zoom * s, self.zoom * s * f, self.zoom * c]
    }

    fn inverse(&self) -> [f32; 4] {
        let m = self.matrix();
        let det = m[0] * m[3] - m[1] * m[2];
        [m[3] / det, -m[1] / det, -m[2] / det, m[0] / det]
    }

    pub fn to_screen(&self, c: (f32, f32), win: (i32, i32)) -> (f32, f32) {
        let m = self.matrix();
        let (dx, dy) = (c.0 - self.center.0, c.1 - self.center.1);
        (m[0] * dx + m[1] * dy + win.0 as f32 / 2.0, m[2] * dx + m[3] * dy + win.1 as f32 / 2.0)
    }

    pub fn to_canvas(&self, s: (f32, f32), win: (i32, i32)) -> (f32, f32) {
        let m = self.inverse();
        let (dx, dy) = (s.0 - win.0 as f32 / 2.0, s.1 - win.1 as f32 / 2.0);
        (m[0] * dx + m[1] * dy + self.center.0, m[2] * dx + m[3] * dy + self.center.1)
    }

    /// 改缩放比例，并保持屏幕上 `anchor` 处的画布点不动（以光标为中心缩放）。
    pub fn zoom_at(&mut self, zoom: f32, anchor: (f32, f32), win: (i32, i32)) {
        let fixed = self.to_canvas(anchor, win);
        self.zoom = zoom.clamp(0.02, 64.0);
        let now = self.to_canvas(anchor, win);
        self.center.0 += fixed.0 - now.0;
        self.center.1 += fixed.1 - now.1;
    }

    /// 按屏幕上的位移平移。
    pub fn pan(&mut self, d: (f32, f32)) {
        let m = self.inverse();
        self.center.0 -= m[0] * d.0 + m[1] * d.1;
        self.center.1 -= m[2] * d.0 + m[3] * d.1;
    }

    /// 让整张画布居中并完整显示，四周留一点边。
    pub fn fit(&mut self, doc: (i32, i32), win: (i32, i32)) {
        let (s, c) = self.angle.sin_cos();
        // 旋转后的外接矩形
        let bw = doc.0 as f32 * c.abs() + doc.1 as f32 * s.abs();
        let bh = doc.0 as f32 * s.abs() + doc.1 as f32 * c.abs();
        self.zoom = (win.0 as f32 * 0.92 / bw).min(win.1 as f32 * 0.92 / bh).clamp(0.02, 64.0);
        self.center = (doc.0 as f32 / 2.0, doc.1 as f32 / 2.0);
    }

    /// 画布矩形在屏幕上的外接矩形（向外取整并留一个像素）。
    pub fn screen_bounds(&self, r: Rect, win: (i32, i32)) -> Rect {
        if r.is_empty() {
            return Rect::EMPTY;
        }
        let corners = [(r.x0, r.y0), (r.x1, r.y0), (r.x0, r.y1), (r.x1, r.y1)].map(|(x, y)| self.to_screen((x as f32, y as f32), win));
        let fold = |f: fn(f32, f32) -> f32, i: usize| corners.iter().map(|c| if i == 0 { c.0 } else { c.1 }).reduce(f).unwrap();
        Rect {
            x0: fold(f32::min, 0).floor() as i32 - 1,
            y0: fold(f32::min, 1).floor() as i32 - 1,
            x1: fold(f32::max, 0).ceil() as i32 + 2,
            y1: fold(f32::max, 1).ceil() as i32 + 2,
        }
    }
}

/// 每个分块合成后的显示像素缓存。笔画只让它碰到的分块失效。
#[derive(Default)]
pub struct Display {
    cache: HashMap<TileCoord, Vec<u32>>,
    /// 暂时不画选区的边线（选区本身还在起作用）
    pub hide_selection: bool,
}

impl Display {
    pub fn invalidate(&mut self, canvas: Rect) {
        for tc in tiles_in(canvas) {
            self.cache.remove(&tc);
        }
    }

    pub fn invalidate_all(&mut self) {
        self.cache.clear();
    }

    /// 把视口里的矩形 `rect`（视口内坐标）画进 `buf`。
    ///
    /// `buf` 是整个窗口的像素（0x00RRGGBB，每行 `stride` 个），视口的左上角在窗口里的 `origin` 处，
    /// 视口大小为 `vp`。视图变换以视口中心为基准。
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        doc: &Document,
        stroke: Option<&Stroke>,
        view: &View,
        vp: (i32, i32),
        buf: &mut [u32],
        stride: usize,
        origin: (i32, i32),
        rect: Rect,
    ) {
        let rect = rect.intersect(Rect { x0: 0, y0: 0, x1: vp.0, y1: vp.1 });
        if rect.is_empty() {
            return;
        }
        // 先把这块屏幕区域会用到的分块都合成好。
        let corners = [(rect.x0, rect.y0), (rect.x1, rect.y0), (rect.x0, rect.y1), (rect.x1, rect.y1)].map(|(x, y)| view.to_canvas((x as f32, y as f32), vp));
        let need = Rect {
            x0: corners.iter().map(|c| c.0).fold(f32::MAX, f32::min).floor() as i32 - 1,
            y0: corners.iter().map(|c| c.1).fold(f32::MAX, f32::min).floor() as i32 - 1,
            x1: corners.iter().map(|c| c.0).fold(f32::MIN, f32::max).ceil() as i32 + 1,
            y1: corners.iter().map(|c| c.1).fold(f32::MIN, f32::max).ceil() as i32 + 1,
        }
        .intersect(doc.bounds());
        let missing: Vec<TileCoord> = tiles_in(need).filter(|tc| !self.cache.contains_key(tc)).collect();
        let compose = |tc: TileCoord| {
            let mut out = vec![0u32; (TILE * TILE) as usize];
            doc.composite_tile(stroke, tc, &mut out);
            (tc, out)
        };
        if missing.len() >= PARALLEL_TILES {
            let per = missing.len().div_ceil(workers());
            let done: Vec<Vec<(TileCoord, Vec<u32>)>> = std::thread::scope(|scope| {
                let jobs: Vec<_> = missing.chunks(per).map(|part| scope.spawn(move || part.iter().map(|&tc| compose(tc)).collect())).collect();
                jobs.into_iter().map(|j| j.join().unwrap()).collect()
            });
            self.cache.extend(done.into_iter().flatten());
        } else {
            self.cache.extend(missing.into_iter().map(compose));
        }

        // 每个屏幕像素取 n×n 个采样点求平均：
        // 缩小显示时 n 随缩小倍数增加，细线不会时有时无；旋转时取 2×2，斜边不出锯齿；
        // 不旋转且不缩小时一个屏幕像素正好对应画布上的整数个像素，取 1 个点就是精确的。
        let rotated = (view.angle.sin().abs() > 1e-4) && (view.angle.cos().abs() > 1e-4);
        let n: i32 = if view.zoom < 0.999 {
            ((1.0 / view.zoom).ceil() as i32).clamp(2, 4)
        } else if rotated {
            2
        } else {
            1
        };
        let inv = view.inverse();
        let taps: Vec<(f32, f32)> = (0..n * n)
            .map(|k| {
                let (ox, oy) = (((k % n) as f32 + 0.5) / n as f32 - 0.5, ((k / n) as f32 + 0.5) / n as f32 - 0.5);
                (inv[0] * ox + inv[1] * oy, inv[2] * ox + inv[3] * oy)
            })
            .collect();
        let (w, h) = (doc.width, doc.height);
        let cache = &self.cache;
        let taps = &taps;
        // 画其中的一段屏幕行。`rows` 是窗口缓冲里从第 `first` 行（视口内坐标）开始的若干整行。
        let paint = |first: i32, rows: &mut [u32]| {
            let mut cur: Option<(TileCoord, &Vec<u32>)> = None;
            let mut fetch = |x: f32, y: f32| -> u32 {
                let (cx, cy) = (x.floor() as i32, y.floor() as i32);
                if cx < 0 || cy < 0 || cx >= w || cy >= h {
                    return PASTEBOARD;
                }
                let tc = (cx >> 6, cy >> 6);
                let tile = match cur {
                    Some((c, t)) if c == tc => t,
                    _ => {
                        let t = &cache[&tc];
                        cur = Some((tc, t));
                        t
                    }
                };
                tile[((cy & 63) * TILE + (cx & 63)) as usize]
            };
            for (k, line) in rows.chunks_mut(stride).enumerate() {
                let sy = first + k as i32;
                let start = view.to_canvas((rect.x0 as f32 + 0.5, sy as f32 + 0.5), vp);
                let at = (origin.0 + rect.x0) as usize;
                let row = &mut line[at..at + (rect.x1 - rect.x0) as usize];
                for (i, out) in row.iter_mut().enumerate() {
                    let (bx, by) = (start.0 + inv[0] * i as f32, start.1 + inv[2] * i as f32);
                    if n == 1 {
                        *out = fetch(bx, by);
                        continue;
                    }
                    let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                    for &(dx, dy) in taps {
                        let v = fetch(bx + dx, by + dy);
                        r += (v >> 16) & 0xFF;
                        g += (v >> 8) & 0xFF;
                        b += v & 0xFF;
                    }
                    let k = (n * n) as u32;
                    *out = ((r / k) << 16) | ((g / k) << 8) | (b / k);
                }
            }
        };
        // 要画的那些屏幕行在缓冲里是连续的一段，按整行切开，各线程各画各的。
        let lines = (rect.y1 - rect.y0) as usize;
        let from = (origin.1 + rect.y0) as usize * stride;
        let end = (from + lines * stride).min(buf.len());
        let area = &mut buf[from..end];
        if (rect.x1 - rect.x0) as i64 * lines as i64 * (n * n) as i64 >= PARALLEL_PIXELS {
            let per = lines.div_ceil(workers());
            let paint = &paint;
            std::thread::scope(|scope| {
                for (j, part) in area.chunks_mut(per * stride).enumerate() {
                    scope.spawn(move || paint(rect.y0 + (j * per) as i32, part));
                }
            });
        } else {
            paint(rect.y0, area);
        }

        if let (Some(sel), false) = (doc.selection(), self.hide_selection) {
            outline(sel, view, vp, buf, stride, origin, rect);
        }
    }
}

/// 选区的边线：屏幕上相邻的两个像素一个在选区里、一个不在，这里就是边。
/// 画成一个像素宽、黑白相间的虚线，在任何底色上都看得见；判定只看屏幕位置，
/// 所以不管这次重画的是哪一块，拼起来都是同一条线。
fn outline(sel: &Selection, view: &View, vp: (i32, i32), buf: &mut [u32], stride: usize, origin: (i32, i32), rect: Rect) {
    let area = view.screen_bounds(sel.bounds(), vp).intersect(rect);
    if area.is_empty() {
        return;
    }
    let inv = view.inverse();
    let mut cur: Option<(TileCoord, Option<&[u16]>)> = None;
    let w = (area.x1 - area.x0) as usize + 1;
    // 每行多算右边一个像素，再多算下面一行，用来和右邻、下邻比较。
    let mut row = |sy: i32, out: &mut [bool]| {
        let start = view.to_canvas((area.x0 as f32 + 0.5, sy as f32 + 0.5), vp);
        for (i, o) in out.iter_mut().enumerate() {
            let (cx, cy) = ((start.0 + inv[0] * i as f32).floor() as i32, (start.1 + inv[2] * i as f32).floor() as i32);
            let tc = (cx >> 6, cy >> 6);
            let tile = match cur {
                Some((c, t)) if c == tc => t,
                _ => {
                    let t = sel.tile(tc);
                    cur = Some((tc, t));
                    t
                }
            };
            *o = tile.is_some_and(|t| t[((cy & 63) * TILE + (cx & 63)) as usize] as u32 >= ONE / 2);
        }
    };
    let (mut here, mut below) = (vec![false; w], vec![false; w]);
    row(area.y0, &mut here);
    for sy in area.y0..area.y1 {
        row(sy + 1, &mut below);
        let at = ((origin.1 + sy) as usize) * stride + (origin.0 + area.x0) as usize;
        for i in 0..w - 1 {
            if here[i] != here[i + 1] || here[i] != below[i] {
                buf[at + i] = if ((area.x0 + i as i32 + sy) >> 2) & 1 == 0 { 0x0010_1010 } else { 0x00FF_FFFF };
            }
        }
        std::mem::swap(&mut here, &mut below);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: (i32, i32) = (800, 600);

    fn view() -> View {
        View { zoom: 1.7, angle: 0.65, flip: true, center: (300.0, 200.0) }
    }

    #[test]
    fn screen_and_canvas_round_trip() {
        let v = view();
        for p in [(0.0, 0.0), (123.5, 77.25), (-40.0, 900.0)] {
            let q = v.to_canvas(v.to_screen(p, WIN), WIN);
            assert!((q.0 - p.0).abs() < 1e-2 && (q.1 - p.1).abs() < 1e-2, "{p:?} -> {q:?}");
        }
    }

    #[test]
    fn zoom_keeps_the_anchor_fixed() {
        let mut v = view();
        let anchor = (610.0, 140.0);
        let before = v.to_canvas(anchor, WIN);
        v.zoom_at(4.0, anchor, WIN);
        let after = v.to_canvas(anchor, WIN);
        assert!((before.0 - after.0).abs() < 1e-2 && (before.1 - after.1).abs() < 1e-2);
    }

    #[test]
    fn pan_moves_content_with_the_pointer() {
        let mut v = view();
        let p = (250.0, 180.0);
        let s0 = v.to_screen(p, WIN);
        v.pan((30.0, -12.0));
        let s1 = v.to_screen(p, WIN);
        assert!((s1.0 - s0.0 - 30.0).abs() < 1e-2 && (s1.1 - s0.1 + 12.0).abs() < 1e-2);
    }

    #[test]
    fn unrotated_full_zoom_maps_pixels_one_to_one() {
        let doc = Document::new(128, 128);
        let v = View { zoom: 1.0, angle: 0.0, flip: false, center: (64.0, 64.0) };
        let win = (200, 200);
        let mut buf = vec![0u32; 200 * 200];
        let mut d = Display::default();
        d.render(&doc, None, &v, win, &mut buf, 200, (0, 0), Rect { x0: 0, y0: 0, x1: 200, y1: 200 });
        // 画布占窗口正中 128×128，外面是底色。
        assert_eq!(buf[(100 * 200 + 100) as usize], 0x00FF_FFFF);
        assert_eq!(buf[(100 * 200 + 36) as usize], 0x00FF_FFFF);
        assert_eq!(buf[(100 * 200 + 35) as usize], PASTEBOARD);
        assert_eq!(buf[(100 * 200 + 163) as usize], 0x00FF_FFFF);
        assert_eq!(buf[(100 * 200 + 164) as usize], PASTEBOARD);
    }

    #[test]
    fn viewport_can_sit_anywhere_in_the_window() {
        let doc = Document::new(64, 64);
        let v = View { zoom: 1.0, angle: 0.0, flip: false, center: (32.0, 32.0) };
        // 窗口 300×200，视口是其中左上角在 (100, 50)、大小 100×100 的一块。
        let mut buf = vec![0x0012_3456u32; 300 * 200];
        let mut d = Display::default();
        d.render(&doc, None, &v, (100, 100), &mut buf, 300, (100, 50), Rect { x0: 0, y0: 0, x1: 100, y1: 100 });
        assert_eq!(buf[(100 * 300 + 150) as usize], 0x00FF_FFFF, "视口中心是画布");
        assert_eq!(buf[(55 * 300 + 105) as usize], PASTEBOARD, "视口内、画布外是底色");
        assert_eq!(buf[(100 * 300 + 99) as usize], 0x0012_3456, "视口外不动");
        assert_eq!(buf[(49 * 300 + 150) as usize], 0x0012_3456);
    }

    #[test]
    fn thin_lines_survive_zooming_out() {
        use crate::stabilizer::Sample;
        use crate::stroke::{BrushParams, Stroke as S};
        let mut doc = Document::new(256, 256);
        // 一条 1 像素宽的竖线，落在奇数列上。
        let (mut s, _) = S::begin(BrushParams { size: 1.0, ..Default::default() }, 0, Sample { x: 101.5, y: 10.0, p: 1.0 });
        s.add(Sample { x: 101.5, y: 240.0, p: 1.0 });
        s.end();
        doc.commit_stroke(&s);
        let v = View { zoom: 0.5, angle: 0.0, flip: false, center: (128.0, 128.0) };
        let mut buf = vec![0u32; 128 * 128];
        let mut d = Display::default();
        d.render(&doc, None, &v, (128, 128), &mut buf, 128, (0, 0), Rect { x0: 0, y0: 0, x1: 128, y1: 128 });
        // 缩到一半后它应该变成一条半灰的线，而不是整条消失。
        let px = buf[(64 * 128 + 50) as usize] & 0xFF;
        assert!(px > 60 && px < 200, "{px}");
    }

    #[test]
    fn selection_outline_is_drawn_on_its_border_only() {
        let mut doc = Document::new(128, 128);
        doc.set_selection(Some(Selection::rect(Rect { x0: 40, y0: 40, x1: 80, y1: 80 })));
        let v = View { zoom: 1.0, angle: 0.0, flip: false, center: (64.0, 64.0) };
        let all = Rect { x0: 0, y0: 0, x1: 128, y1: 128 };
        let mut buf = vec![0u32; 128 * 128];
        let mut d = Display::default();
        d.render(&doc, None, &v, (128, 128), &mut buf, 128, (0, 0), all);
        let marked: Vec<usize> = (0..buf.len()).filter(|&i| buf[i] != 0x00FF_FFFF).collect();
        // 一圈一个像素宽的边：大约是周长那么多个像素，其中深色的占一半左右，这里只数深色的。
        assert!(marked.len() > 60 && marked.len() < 100, "{}", marked.len());
        assert_eq!(buf[60 * 128 + 60], 0x00FF_FFFF, "选区里面不画");
        assert_eq!(buf[20 * 128 + 20], 0x00FF_FFFF, "选区外面不画");
        // 分两块重画，结果和一次画完一样。
        let mut halves = vec![0u32; 128 * 128];
        d.render(&doc, None, &v, (128, 128), &mut halves, 128, (0, 0), Rect { x1: 61, ..all });
        d.render(&doc, None, &v, (128, 128), &mut halves, 128, (0, 0), Rect { x0: 61, ..all });
        assert!(buf == halves);
        // 关掉显示后不画边线，选区还在。
        d.hide_selection = true;
        d.render(&doc, None, &v, (128, 128), &mut buf, 128, (0, 0), all);
        assert!(buf.iter().all(|&p| p == 0x00FF_FFFF));
        assert!(doc.selection().is_some());
    }

    #[test]
    fn large_redraws_split_across_threads_give_the_same_picture() {
        use crate::stabilizer::Sample;
        use crate::stroke::{BrushParams, Stroke as S};
        let mut doc = Document::new(1600, 1200);
        for k in 0..12 {
            let y = 50.0 + 90.0 * k as f32;
            let (mut s, _) = S::begin(BrushParams { size: 30.0, color: [20 * k as u8, 90, 200], ..Default::default() }, 0, Sample { x: 40.0, y, p: 1.0 });
            s.add(Sample { x: 1500.0, y: y + 60.0, p: 1.0 });
            s.end();
            doc.commit_stroke(&s);
        }
        // 同一个视图，一次整张画（够大，会分给几个线程），一次切成很多横条画（每条都小，在当前线程上画）。
        // 横条和整张的左边界相同，所以每一行的浮点运算一模一样，结果应当逐像素相同。
        for v in [View { zoom: 1.0, angle: 0.0, flip: false, center: (800.0, 600.0) }, View { zoom: 0.6, angle: 0.4, flip: true, center: (700.0, 650.0) }] {
            let (vw, vh) = (1400usize, 1000usize);
            let (mut whole, mut pieces) = (vec![0u32; vw * vh], vec![0u32; vw * vh]);
            let vp = (vw as i32, vh as i32);
            Display::default().render(&doc, None, &v, vp, &mut whole, vw, (0, 0), Rect { x0: 0, y0: 0, x1: vp.0, y1: vp.1 });
            let mut d = Display::default();
            for y in (0..vp.1).step_by(50) {
                d.render(&doc, None, &v, vp, &mut pieces, vw, (0, 0), Rect { x0: 0, y0: y, x1: vp.0, y1: y + 50 });
            }
            assert!(whole == pieces);
        }
    }
}

#[cfg(test)]
mod timing {
    use super::*;
    use crate::stabilizer::Sample;
    use crate::stroke::{BrushParams, Stroke};
    use std::time::Instant;

    /// 大笔刷每来一个样本要花多久：笔画引擎、合成、显示各占多少。平时不跑：
    /// `cargo test --release -p zhibi-core big_brush_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn big_brush_timing() {
        let cases = [(500.0, false, 5.0, 1.0), (500.0, true, 5.0, 1.0), (500.0, true, 20.0, 1.0), (500.0, true, 20.0, 0.56), (100.0, false, 5.0, 1.0), (10.0, false, 5.0, 1.0)];
        for (size, marker, step, zoom) in cases {
            let mut doc = Document::new(3000, 2000);
            doc.fill_active([255, 255, 255]);
            doc.add_layer();
            let view = View { zoom, angle: 0.0, flip: false, center: (1500.0, 1000.0) };
            let vp = (1816, 1407);
            let mut buf = vec![0u32; (vp.0 * vp.1) as usize];
            let mut display = Display::default();
            display.render(&doc, None, &view, vp, &mut buf, vp.0 as usize, (0, 0), Rect { x0: 0, y0: 0, x1: vp.0, y1: vp.1 });
            let params = BrushParams { size, marker, density: if marker { 0.5 } else { 1.0 }, hardness: if marker { 0.9 } else { 1.0 }, ..Default::default() };
            let at = |k: usize| Sample { x: 900.0 + step * k as f32, y: 1000.0 + 80.0 * (k as f32 * 0.05).sin(), p: 1.0 };
            let (mut stroke, _) = Stroke::begin(params, 3, at(0));
            let (mut t_add, mut t_draw, mut worst) = (0.0f64, 0.0f64, 0.0f64);
            let n = 120;
            for k in 1..=n {
                let t0 = Instant::now();
                let dirty = stroke.add(at(k));
                let t1 = Instant::now();
                display.invalidate(dirty);
                let screen = view.screen_bounds(dirty, vp);
                display.render(&doc, Some(&stroke), &view, vp, &mut buf, vp.0 as usize, (0, 0), screen);
                let t2 = Instant::now();
                t_add += (t1 - t0).as_secs_f64();
                t_draw += (t2 - t1).as_secs_f64();
                worst = worst.max((t2 - t0).as_secs_f64());
            }
            let t0 = Instant::now();
            stroke.end();
            doc.commit_stroke(&stroke);
            let t_commit = t0.elapsed().as_secs_f64();
            println!(
                "size {size:>5} marker {marker:<5} step {step:>4} zoom {zoom:<4}: per sample engine {:.2} ms, redraw {:.2} ms, worst {:.2} ms; commit {:.1} ms",
                t_add * 1e3 / n as f64,
                t_draw * 1e3 / n as f64,
                worst * 1e3,
                t_commit * 1e3
            );
        }
    }
}
