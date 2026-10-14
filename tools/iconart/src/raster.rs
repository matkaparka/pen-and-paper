// SPDX-License-Identifier: GPL-3.0-or-later
//! 一个很小的抗锯齿光栅器：形状全部用有符号距离函数（SDF）描述，
//! 边缘按距离做 1 像素宽的解析覆盖率；软阴影用"半平面卷高斯"的覆盖率近似。
//! 像素按预乘 alpha 存成 f32，最后用面积平均缩小到各个图标尺寸。

use std::f32::consts::SQRT_2;

#[derive(Clone, Copy, Debug)]
pub struct V2 {
    pub x: f32,
    pub y: f32,
}

pub const fn v(x: f32, y: f32) -> V2 {
    V2 { x, y }
}

impl V2 {
    pub fn sub(self, o: V2) -> V2 {
        v(self.x - o.x, self.y - o.y)
    }
    pub fn mul(self, k: f32) -> V2 {
        v(self.x * k, self.y * k)
    }
    pub fn dot(self, o: V2) -> f32 {
        self.x * o.x + self.y * o.y
    }
    pub fn len(self) -> f32 {
        self.dot(self).sqrt()
    }
}

/// 非预乘的颜色，分量 0..1（sRGB 空间，合成也直接在 sRGB 里做，和常见的设计软件一致）。
#[derive(Clone, Copy, Debug)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// 0xRRGGBB → 不透明颜色
pub fn hex(rgb: u32) -> Rgba {
    Rgba {
        r: ((rgb >> 16) & 255) as f32 / 255.0,
        g: ((rgb >> 8) & 255) as f32 / 255.0,
        b: (rgb & 255) as f32 / 255.0,
        a: 1.0,
    }
}

impl Rgba {
    pub fn alpha(self, a: f32) -> Rgba {
        Rgba { a: self.a * a, ..self }
    }
    pub fn mix(self, o: Rgba, t: f32) -> Rgba {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Rgba {
            r: l(self.r, o.r),
            g: l(self.g, o.g),
            b: l(self.b, o.b),
            a: l(self.a, o.a),
        }
    }
}

/// 相似变换：局部坐标 → 画布坐标（旋转 + 均匀缩放 + 平移）。角度按屏幕方向（y 向下）顺时针为正，单位是度。
#[derive(Clone, Copy, Debug)]
pub struct Xf {
    pub o: V2,
    c: f32,
    s: f32,
    /// 缩放倍数：局部的 1 个单位 = 画布上 k 个像素
    pub k: f32,
}

impl Xf {
    pub fn new(x: f32, y: f32, deg: f32) -> Xf {
        let r = deg.to_radians();
        Xf {
            o: v(x, y),
            c: r.cos(),
            s: r.sin(),
            k: 1.0,
        }
    }
    pub fn scaled(self, k: f32) -> Xf {
        Xf { k, ..self }
    }
    /// 整体平移（画阴影用）
    pub fn shifted(self, dx: f32, dy: f32) -> Xf {
        Xf {
            o: v(self.o.x + dx, self.o.y + dy),
            ..self
        }
    }
    pub fn to_world(self, p: V2) -> V2 {
        let (x, y) = (p.x * self.k, p.y * self.k);
        v(self.o.x + self.c * x - self.s * y, self.o.y + self.s * x + self.c * y)
    }
    pub fn to_local(self, w: V2) -> V2 {
        let d = w.sub(self.o).mul(1.0 / self.k);
        v(self.c * d.x + self.s * d.y, -self.s * d.x + self.c * d.y)
    }
}

/// 局部坐标下的包围盒 [x0, y0, x1, y1]；只用来限定要扫描的像素范围。
pub type Bb = [f32; 4];

// ---------- SDF 基本形状（负值在形状内部，单位是像素） ----------

/// 以 (cx, cy) 为中心、半宽 hx、半高 hy、圆角 r 的圆角矩形
pub fn rbox(p: V2, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let r = r.min(hx).min(hy);
    let qx = (p.x - cx).abs() - hx + r;
    let qy = (p.y - cy).abs() - hy + r;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// 沿 x 轴从 x0 到 x1、上下对称、半宽 hw 的圆角矩形（笔杆一类的形状）
pub fn bar(p: V2, x0: f32, x1: f32, hw: f32, r: f32) -> f32 {
    rbox(p, (x0 + x1) * 0.5, 0.0, (x1 - x0) * 0.5, hw, r)
}

/// 线段 a–b 加粗到半径 r 的胶囊形
pub fn capsule(p: V2, a: V2, b: V2, r: f32) -> f32 {
    let pa = p.sub(a);
    let ba = b.sub(a);
    let h = (pa.dot(ba) / ba.dot(ba)).clamp(0.0, 1.0);
    pa.sub(ba.mul(h)).len() - r
}

pub fn circle(p: V2, c: V2, r: f32) -> f32 {
    p.sub(c).len() - r
}

/// 任意简单多边形的精确距离（按奇偶规则判断内外）
pub fn poly(p: V2, pts: &[V2]) -> f32 {
    let n = pts.len();
    let mut d = p.sub(pts[0]).dot(p.sub(pts[0]));
    let mut s = 1.0f32;
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (pts[i], pts[j]);
        let e = b.sub(a);
        let w = p.sub(a);
        let h = (w.dot(e) / e.dot(e)).clamp(0.0, 1.0);
        let q = w.sub(e.mul(h));
        d = d.min(q.dot(q));
        let c1 = p.y >= a.y;
        let c2 = p.y < b.y;
        let c3 = e.x * w.y > e.y * w.x;
        if (c1 && c2 && c3) || (!c1 && !c2 && !c3) {
            s = -s;
        }
        j = i;
    }
    s * d.sqrt()
}

/// 沿 x 轴、上下对称的梯形：x0 处半宽 h0，x1 处半宽 h1
pub fn taper(p: V2, x0: f32, x1: f32, h0: f32, h1: f32) -> f32 {
    poly(p, &[v(x0, -h0), v(x1, -h1), v(x1, h1), v(x0, h0)])
}

/// y > y0 的半平面（局部坐标）
pub fn below(p: V2, y0: f32) -> f32 {
    y0 - p.y
}

/// 平滑并集：两个形状相接处圆滑过渡，过渡宽度约 k 像素（画颜料、液滴用）
pub fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (k - (a - b).abs()).max(0.0) / k;
    a.min(b) - h * h * k * 0.25
}

/// 形状内侧宽 w 的描边带
pub fn inner_ring(d: f32, w: f32) -> f32 {
    d.max(-(d + w))
}

// ---------- 画布 ----------

/// 方形画布，像素是预乘 alpha 的 [r, g, b, a]。
pub struct Canvas {
    pub n: usize,
    pub px: Vec<[f32; 4]>,
}

fn erf(x: f64) -> f64 {
    // Abramowitz–Stegun 7.1.26，误差 < 1.5e-7，画阴影绰绰有余
    let s = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let poly =
        ((((1.061_405_429 * t - 1.453_152_027) * t + 1.421_413_741) * t - 0.284_496_736) * t + 0.254_829_592) * t;
    s * (1.0 - poly * (-x * x).exp())
}

impl Canvas {
    pub fn new(n: usize) -> Canvas {
        Canvas {
            n,
            px: vec![[0.0; 4]; n * n],
        }
    }

    /// 把一个形状合成到画布上。
    /// `soft == 0`：边缘做 1 像素解析抗锯齿；`soft > 0`：边缘按 sigma = soft 像素的高斯模糊，用来画软阴影。
    /// `paint` 拿局部坐标返回非预乘颜色，可以做渐变或纹理。
    pub fn shade(&mut self, xf: &Xf, bb: Bb, sdf: impl Fn(V2) -> f32, paint: impl Fn(V2) -> Rgba, soft: f32) {
        let margin = 2.0 + soft * 3.5;
        let corners = [v(bb[0], bb[1]), v(bb[2], bb[1]), v(bb[2], bb[3]), v(bb[0], bb[3])];
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for c in corners {
            let w = xf.to_world(c);
            x0 = x0.min(w.x);
            y0 = y0.min(w.y);
            x1 = x1.max(w.x);
            y1 = y1.max(w.y);
        }
        let n = self.n as f32;
        let ix0 = (x0 - margin).floor().clamp(0.0, n) as usize;
        let iy0 = (y0 - margin).floor().clamp(0.0, n) as usize;
        let ix1 = (x1 + margin).ceil().clamp(0.0, n) as usize;
        let iy1 = (y1 + margin).ceil().clamp(0.0, n) as usize;
        let k = if soft > 0.0 {
            1.0 / (soft as f64 * SQRT_2 as f64)
        } else {
            0.0
        };
        for iy in iy0..iy1 {
            for ix in ix0..ix1 {
                let p = xf.to_local(v(ix as f32 + 0.5, iy as f32 + 0.5));
                let d = sdf(p) * xf.k;
                if d > margin {
                    continue;
                }
                let cov = if soft > 0.0 {
                    (0.5 * (1.0 - erf(d as f64 * k))) as f32
                } else {
                    (0.5 - d).clamp(0.0, 1.0)
                };
                if cov <= 0.0 {
                    continue;
                }
                let c = paint(p);
                let a = c.a * cov;
                if a <= 0.0 {
                    continue;
                }
                let dst = &mut self.px[iy * self.n + ix];
                let inv = 1.0 - a;
                dst[0] = c.r * a + dst[0] * inv;
                dst[1] = c.g * a + dst[1] * inv;
                dst[2] = c.b * a + dst[2] * inv;
                dst[3] = a + dst[3] * inv;
            }
        }
    }

    /// 纯色形状
    pub fn fill(&mut self, xf: &Xf, bb: Bb, sdf: impl Fn(V2) -> f32, color: Rgba) {
        self.shade(xf, bb, sdf, |_| color, 0.0);
    }

    /// 软阴影：形状整体平移 (dx, dy)，边缘高斯模糊 sigma 像素
    pub fn shadow(&mut self, xf: &Xf, bb: Bb, sdf: impl Fn(V2) -> f32, d: (f32, f32), sigma: f32, color: Rgba) {
        self.shade(&xf.shifted(d.0, d.1), bb, sdf, |_| color, sigma);
    }

    /// 面积平均缩小到 m×m（两遍可分离的盒式滤波，源像素按重叠长度加权）
    pub fn downscale(&self, m: usize) -> Canvas {
        let w = weights(self.n, m);
        let n = self.n;
        let mut tmp = vec![[0.0f32; 4]; m * n];
        for y in 0..n {
            for (ox, ws) in w.iter().enumerate() {
                let mut acc = [0.0f32; 4];
                for &(sx, wt) in ws {
                    let s = self.px[y * n + sx];
                    for c in 0..4 {
                        acc[c] += s[c] * wt;
                    }
                }
                tmp[y * m + ox] = acc;
            }
        }
        let mut out = Canvas::new(m);
        for (oy, ws) in w.iter().enumerate() {
            for ox in 0..m {
                let mut acc = [0.0f32; 4];
                for &(sy, wt) in ws {
                    let s = tmp[sy * m + ox];
                    for c in 0..4 {
                        acc[c] += s[c] * wt;
                    }
                }
                out.px[oy * m + ox] = acc;
            }
        }
        out
    }

    /// 转成非预乘的 8 位 RGBA（PNG / ICO 用）
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.px.len() * 4);
        for p in &self.px {
            let a = p[3].clamp(0.0, 1.0);
            let q = |c: f32| -> u8 {
                if a <= 0.0 {
                    0
                } else {
                    ((c / a).clamp(0.0, 1.0) * 255.0 + 0.5) as u8
                }
            };
            let a8 = (a * 255.0 + 0.5) as u8;
            if a8 == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                out.extend_from_slice(&[q(p[0]), q(p[1]), q(p[2]), a8]);
            }
        }
        out
    }
}

/// n → m 缩小时每个输出像素对应的源像素及权重（权重和为 1）
fn weights(n: usize, m: usize) -> Vec<Vec<(usize, f32)>> {
    let k = n as f64 / m as f64;
    (0..m)
        .map(|i| {
            let a = i as f64 * k;
            let b = (i + 1) as f64 * k;
            let mut ws = Vec::new();
            let mut s = a.floor() as usize;
            while (s as f64) < b && s < n {
                let lo = a.max(s as f64);
                let hi = b.min((s + 1) as f64);
                if hi > lo {
                    ws.push((s, ((hi - lo) / k) as f32));
                }
                s += 1;
            }
            ws
        })
        .collect()
}
