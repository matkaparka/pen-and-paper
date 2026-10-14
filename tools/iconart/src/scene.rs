// SPDX-License-Identifier: GPL-3.0-or-later
//! 图标画面：一张纸，上面随手放着水笔、涂改液笔、马克笔和一把油漆刷。
//! 所有坐标都在 1024×1024 的画布上，单位是像素；每件工具在自己的局部坐标里画，
//! 局部 x 轴沿工具长度方向（笔尖 / 刷毛在 -x 一端），y 轴横穿工具，上方是受光面。

use crate::raster::*;

/// 阴影颜色：偏暖的深棕，比纯黑柔和
const SHADOW: u32 = 0x3a2a1c;
/// 画布边长
const ART: f32 = 1024.0;

/// 工具投在纸上的软阴影（光从左上方来）
fn tool_shadow(c: &mut Canvas, xf: &Xf, bb: Bb, sil: impl Fn(V2) -> f32 + Copy) {
    c.shadow(xf, bb, sil, (10.0, 16.0), 12.0, hex(SHADOW).alpha(0.26));
    c.shadow(xf, bb, sil, (3.0, 5.0), 3.0, hex(SHADOW).alpha(0.22));
}

/// 圆柱形零件的平面化明暗：下半部压暗一条，上方一道细高光
fn cylinder(c: &mut Canvas, xf: &Xf, bb: Bb, d: impl Fn(V2) -> f32 + Copy, base: Rgba, dark: Rgba, hw: f32) {
    c.fill(xf, bb, d, base);
    c.fill(xf, bb, |p| d(p).max(below(p, hw * 0.28)), dark);
}

// ---------------------------------------------------------------- 纸

pub fn paper(c: &mut Canvas, xf: Xf, hx: f32, hy: f32) {
    let bb = [-hx, -hy, hx, hy];
    let sil = move |p: V2| rbox(p, 0.0, 0.0, hx, hy, 10.0);
    c.shadow(&xf, bb, sil, (12.0, 22.0), 26.0, hex(SHADOW).alpha(0.30));
    c.shadow(&xf, bb, sil, (2.0, 4.0), 4.0, hex(SHADOW).alpha(0.20));
    let top = hex(0xfcf7ee);
    let bot = hex(0xf1e8d8);
    c.shade(
        &xf,
        bb,
        sil,
        |p| {
            let t = ((p.x / hx + p.y / hy) * 0.25 + 0.5).clamp(0.0, 1.0);
            top.mix(bot, t)
        },
        0.0,
    );
    // 边缘一圈很淡的描边，浅色任务栏上也能看出纸的轮廓
    c.fill(&xf, bb, |p| inner_ring(sil(p), 3.0), hex(0xd9cdb8).alpha(0.9));
}

// ---------------------------------------------------------------- 水笔（深蓝）

pub fn gel_pen(c: &mut Canvas, xf: Xf) {
    let bb = [-310.0, -40.0, 310.0, 40.0];
    let cone = |p: V2| taper(p, -294.0, -248.0, 7.0, 19.0);
    let grip = |p: V2| bar(p, -252.0, -118.0, 29.0, 8.0);
    let barrel = |p: V2| bar(p, -124.0, 180.0, 27.0, 4.0);
    let cap = |p: V2| bar(p, 166.0, 302.0, 31.0, 18.0);
    let ball = |p: V2| circle(p, v(-297.0, 0.0), 6.0);
    let sil = move |p: V2| cone(p).min(grip(p)).min(barrel(p)).min(cap(p)).min(ball(p));
    tool_shadow(c, &xf, bb, sil);

    c.fill(&xf, bb, ball, hex(0x4a505a));
    cylinder(c, &xf, bb, cone, hex(0xd3d8df), hex(0xa4acb8), 19.0);
    // 笔杆
    cylinder(c, &xf, bb, barrel, hex(0x2f4bb8), hex(0x243a94), 27.0);
    c.fill(
        &xf,
        bb,
        |p| capsule(p, v(-110.0, -14.0), v(168.0, -14.0), 4.0),
        hex(0x7d93ea).alpha(0.9),
    );
    // 橡胶握把 + 防滑环
    cylinder(c, &xf, bb, grip, hex(0x1f2b72), hex(0x172158), 29.0);
    for i in 0..8 {
        let x = -240.0 + i as f32 * 15.5;
        c.fill(
            &xf,
            bb,
            |p| grip(p).max(rbox(p, x, 0.0, 2.6, 40.0, 1.0)),
            hex(0x111845).alpha(0.8),
        );
    }
    // 笔帽套在尾端，带金属笔夹
    cylinder(c, &xf, bb, cap, hex(0x22318a), hex(0x1a266c), 31.0);
    c.fill(
        &xf,
        bb,
        |p| cap(p).max(rbox(p, 172.0, 0.0, 5.0, 40.0, 1.0)),
        hex(0x5a72d8),
    );
    let clip = |p: V2| rbox(p, 238.0, -6.0, 58.0, 8.5, 8.0).min(circle(p, v(186.0, -6.0), 12.0));
    c.fill(&xf, bb, |p| clip(p) - 3.0, hex(0x1a2050).alpha(0.6));
    c.fill(&xf, bb, clip, hex(0xdfe3e9));
    c.fill(&xf, bb, |p| clip(p).max(below(p, -4.0)), hex(0xb3bac5));
}

// ---------------------------------------------------------------- 涂改液笔（白杆、彩色笔帽）

pub fn correction_pen(c: &mut Canvas, xf: Xf) {
    let bb = [-240.0, -50.0, 240.0, 50.0];
    let needle = |p: V2| bar(p, -232.0, -198.0, 4.0, 4.0);
    let tip = |p: V2| taper(p, -206.0, -168.0, 8.0, 17.0);
    let nozzle = |p: V2| taper(p, -172.0, -126.0, 18.0, 38.0) - 2.0;
    let body = |p: V2| bar(p, -132.0, 124.0, 40.0, 9.0);
    let cap = |p: V2| bar(p, 106.0, 234.0, 44.0, 22.0);
    let sil = move |p: V2| needle(p).min(tip(p)).min(nozzle(p)).min(body(p)).min(cap(p));
    tool_shadow(c, &xf, bb, sil);

    let outline = hex(0x9aa3b0);
    c.fill(&xf, bb, needle, hex(0x9098a4));
    cylinder(c, &xf, bb, tip, hex(0xd5dae1), hex(0xa7afba), 17.0);
    // 白色塑料部分：先画一圈灰色外轮廓，避免白杆在纸上"消失"
    c.fill(&xf, bb, |p| nozzle(p).min(body(p)) - 5.0, outline);
    cylinder(c, &xf, bb, nozzle, hex(0xf8f9fb), hex(0xe1e5ea), 38.0);
    cylinder(c, &xf, bb, body, hex(0xffffff), hex(0xe4e8ee), 40.0);
    c.fill(
        &xf,
        bb,
        |p| capsule(p, v(-115.0, -21.0), v(100.0, -21.0), 4.0),
        hex(0xffffff),
    );
    // 标签：一条彩色细带 + 两行"字"
    c.fill(
        &xf,
        bb,
        |p| body(p).max(rbox(p, -48.0, 0.0, 9.0, 50.0, 0.0)),
        hex(0xf2b33d),
    );
    c.fill(
        &xf,
        bb,
        |p| capsule(p, v(-20.0, -6.0), v(70.0, -6.0), 4.0),
        hex(0xb9c1cc),
    );
    c.fill(
        &xf,
        bb,
        |p| capsule(p, v(-20.0, 10.0), v(40.0, 10.0), 4.0),
        hex(0xb9c1cc),
    );
    // 笔帽套在尾端
    cylinder(c, &xf, bb, cap, hex(0xf4b53f), hex(0xd99a28), 44.0);
    c.fill(
        &xf,
        bb,
        |p| cap(p).max(rbox(p, 114.0, 0.0, 5.0, 50.0, 1.0)),
        hex(0xfbd27e),
    );
    for i in 0..4 {
        let x = 150.0 + i as f32 * 16.0;
        c.fill(
            &xf,
            bb,
            |p| cap(p).max(rbox(p, x, 0.0, 2.5, 50.0, 1.0)),
            hex(0xc4861c).alpha(0.7),
        );
    }
}

// ---------------------------------------------------------------- 马克笔（珊瑚红）

pub fn marker(c: &mut Canvas, xf: Xf) {
    let bb = [-320.0, -56.0, 320.0, 56.0];
    let nib = |p: V2| {
        poly(
            p,
            &[
                v(-262.0, -24.0),
                v(-262.0, 24.0),
                v(-294.0, 24.0),
                v(-312.0, -4.0),
                v(-302.0, -24.0),
            ],
        ) - 2.0
    };
    let collar = |p: V2| taper(p, -268.0, -222.0, 28.0, 42.0) - 2.0;
    let body = |p: V2| bar(p, -228.0, 170.0, 46.0, 10.0);
    let cap = |p: V2| bar(p, 150.0, 312.0, 50.0, 26.0);
    let sil = move |p: V2| nib(p).min(collar(p)).min(body(p)).min(cap(p));
    tool_shadow(c, &xf, bb, sil);

    c.fill(&xf, bb, nib, hex(0xee5b44));
    c.fill(&xf, bb, |p| nib(p).max(below(p, 6.0)), hex(0xc8432f));
    cylinder(c, &xf, bb, collar, hex(0xf1eee9), hex(0xd2cdc5), 42.0);
    cylinder(c, &xf, bb, body, hex(0xf36b51), hex(0xd8513b), 46.0);
    c.fill(
        &xf,
        bb,
        |p| capsule(p, v(-208.0, -25.0), v(150.0, -25.0), 5.0),
        hex(0xfba591).alpha(0.9),
    );
    cylinder(c, &xf, bb, cap, hex(0xc9412f), hex(0xa83424), 50.0);
    c.fill(
        &xf,
        bb,
        |p| cap(p).max(rbox(p, 160.0, 0.0, 8.0, 60.0, 1.0)),
        hex(0xf8dcd2),
    );
    c.fill(
        &xf,
        bb,
        |p| capsule(p, v(180.0, -28.0), v(290.0, -28.0), 5.0),
        hex(0xe2705d).alpha(0.9),
    );
}

// ---------------------------------------------------------------- 油漆刷

/// 木柄半宽沿长度的变化：颈部粗、中段收细、尾部是圆头桨形
fn handle_hw(x: f32) -> f32 {
    let sm = |t: f32| {
        let t = t.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let h = if x < 0.0 {
        50.0
    } else if x < 110.0 {
        50.0 + (34.0 - 50.0) * sm(x / 110.0)
    } else {
        34.0 + (46.0 - 34.0) * sm((x - 110.0) / 190.0)
    };
    let (end, r) = (410.0, 46.0);
    if x > end - r {
        let t = ((x - (end - r)) / r).min(1.0);
        h * (1.0 - t * t).max(0.0).sqrt()
    } else {
        h
    }
}

pub fn brush(c: &mut Canvas, xf: Xf) {
    let bb = [-425.0, -95.0, 420.0, 95.0];
    // 各部分自己的包围盒，少扫一些像素
    let hb = [-85.0, -55.0, 415.0, 55.0];
    let bbr = [-425.0, -90.0, -215.0, 90.0];
    let fb = [-240.0, -92.0, -55.0, 92.0];
    let mut outline = Vec::new();
    let mut x = -80.0;
    while x < 409.5 {
        outline.push(v(x, -handle_hw(x)));
        x += 8.0;
    }
    outline.push(v(409.5, -handle_hw(409.5)));
    let n = outline.len();
    for i in (0..n).rev() {
        let p = outline[i];
        outline.push(v(p.x, -p.y));
    }
    let hole = |p: V2| circle(p, v(364.0, 0.0), 14.0);
    let handle = |p: V2| poly(p, &outline).max(-hole(p));
    let ferrule = |p: V2| taper(p, -226.0, -64.0, 78.0, 68.0) - 6.0;
    // 刷毛外形：两侧略微张开，末端是一条圆润的弧线
    let mut bristle_pts = vec![v(-222.0, -74.0), v(-386.0, -81.0)];
    for i in 0..=16 {
        let y = -81.0 + 162.0 * i as f32 / 16.0;
        bristle_pts.push(v(-418.0 + 30.0 * (y.abs() / 81.0).powi(4), y));
    }
    bristle_pts.extend([v(-386.0, 81.0), v(-222.0, 74.0)]);
    let bristles = |p: V2| poly(p, &bristle_pts) - 4.0;
    let sil = |p: V2| poly(p, &outline).min(ferrule(p)).min(bristles(p));
    tool_shadow(c, &xf, bb, sil);

    // 木柄：暗面、高光、几道木纹、挂孔
    c.fill(&xf, hb, handle, hex(0xe0a96e));
    c.fill(&xf, hb, |p| handle(p).max(below(p, 14.0)), hex(0xc58a4f));
    c.fill(
        &xf,
        hb,
        |p| handle(p).max(capsule(p, v(30.0, -24.0), v(380.0, -30.0), 5.0)),
        hex(0xf0c895),
    );
    for (y0, amp, ph) in [(-14.0f32, 4.0f32, 0.0f32), (4.0, 5.0, 1.7), (26.0, 3.0, 3.1)] {
        c.fill(
            &xf,
            hb,
            |p| {
                (handle(p) + 9.0)
                    .max((p.y - (y0 + amp * (p.x * 0.018 + ph).sin())).abs() - 2.4)
                    .max(-p.x + 20.0)
            },
            hex(0xa86e38).alpha(0.45),
        );
    }
    c.fill(&xf, hb, |p| (hole(p) - 4.0).max(-hole(p)), hex(0x9a6431).alpha(0.7));

    // 刷毛（浅色天然毛）+ 毛丝纹理
    c.fill(&xf, bbr, bristles, hex(0xead6aa));
    c.fill(&xf, bbr, |p| bristles(p).max(below(p, 26.0)), hex(0xd6bd8a));
    let strands = |c: &mut Canvas, region: &dyn Fn(V2) -> f32, color: Rgba| {
        for k in -5..=5 {
            let yk = k as f32 * 13.5;
            c.fill(
                &xf,
                bbr,
                |p| (region(p) + 3.0).max((p.y - yk - 2.0 * (p.x * 0.05 + k as f32).sin()).abs() - 1.4),
                color,
            );
        }
    };
    strands(c, &bristles, hex(0xb89a62).alpha(0.45));
    // 刷尖蘸的颜料：边缘起伏，有两处顺着刷毛往上渗，用平滑并集连成一片
    let paint = |p: V2| {
        let main = p.x - (-338.0 + 9.0 * (0.05 * p.y + 0.6).sin());
        let drip1 = capsule(p, v(-340.0, -26.0), v(-306.0, -28.0), 10.0);
        let drip2 = capsule(p, v(-340.0, 40.0), v(-318.0, 41.0), 8.0);
        bristles(p).max(smin(smin(main, drip1, 18.0), drip2, 18.0))
    };
    c.fill(&xf, bbr, paint, hex(0x1fa595));
    c.fill(&xf, bbr, |p| paint(p).max(below(p, 24.0)), hex(0x178676));
    strands(c, &paint, hex(0x0f6b5f).alpha(0.35));
    c.fill(
        &xf,
        bbr,
        |p| capsule(p, v(-398.0, -46.0), v(-358.0, -52.0), 5.5),
        hex(0x8be0d3).alpha(0.9),
    );
    c.fill(&xf, bbr, |p| circle(p, v(-312.0, -30.0), 3.5), hex(0x8be0d3).alpha(0.9));

    // 金属箍：受光面高光、两道压痕、靠刷毛一侧的卷边
    let fe = ferrule;
    c.fill(&xf, fb, fe, hex(0xc9ced6));
    c.fill(&xf, fb, |p| fe(p).max(below(p, 26.0)), hex(0xa4acb8));
    c.fill(
        &xf,
        fb,
        |p| fe(p).max(rbox(p, -145.0, -54.0, 100.0, 9.0, 0.0)),
        hex(0xf1f3f6),
    );
    for gx in [-108.0f32, -90.0] {
        c.fill(
            &xf,
            fb,
            |p| fe(p).max(rbox(p, gx, 0.0, 3.2, 100.0, 1.0)),
            hex(0x7e8794).alpha(0.75),
        );
        c.fill(
            &xf,
            fb,
            |p| fe(p).max(rbox(p, gx + 5.5, 0.0, 2.0, 100.0, 1.0)),
            hex(0xf4f6f8).alpha(0.8),
        );
    }
    c.fill(
        &xf,
        fb,
        |p| fe(p).max(rbox(p, -222.0, 0.0, 7.0, 100.0, 1.0)),
        hex(0x959dab),
    );
}

// ---------------------------------------------------------------- 完整画面

pub fn full(c: &mut Canvas) {
    // 整个画面围绕画布中心放大一点，让图标在小尺寸下更饱满
    let cam = |x: f32, y: f32, deg: f32| {
        let (k, cx, cy) = (1.08, 512.0, 520.0);
        Xf::new(cx + (x - cx) * k, cy - 14.0 + (y - cy) * k, deg).scaled(k)
    };
    paper(c, cam(505.0, 515.0, -7.0), 300.0, 375.0);
    gel_pen(c, cam(470.0, 285.0, -16.0));
    marker(c, cam(734.0, 575.0, -80.0));
    brush(c, cam(520.0, 580.0, -42.0));
    correction_pen(c, cam(415.0, 498.0, 12.0));
}

// ---------------------------------------------------------------- 小尺寸简化版（≤ 24 像素）

/// 小图标：纸摆正、纸边和横竖方向的工具边缘对齐目标尺寸的像素格，工具加粗，
/// 只保留水笔（上方横放）、油漆刷（45° 斜放）和马克笔（右侧竖放），构图和大图一致。
pub fn small(c: &mut Canvas, n: usize) {
    let u = ART / n as f32; // 目标尺寸的一个像素在画布上的宽度
    let sn = |f: f32| (f * n as f32).round() * u; // 比例坐标 → 对齐到像素格的画布坐标
    let id = Xf::new(0.0, 0.0, 0.0);
    let all = [0.0, 0.0, ART, ART];

    // 纸
    let (px0, px1, py0, py1) = (sn(0.125), sn(0.875), sn(0.0625), sn(0.9375));
    let paper = |p: V2| {
        rbox(
            p,
            (px0 + px1) * 0.5,
            (py0 + py1) * 0.5,
            (px1 - px0) * 0.5,
            (py1 - py0) * 0.5,
            u * 0.35,
        )
    };
    c.fill(&id.shifted(u, u), all, paper, hex(SHADOW).alpha(0.22));
    c.fill(&id, all, paper, hex(0xfcf8ef));
    c.fill(&id, all, |p| inner_ring(paper(p), u), hex(0xcdbfa6));

    let shadow = |c: &mut Canvas, sil: &dyn Fn(V2) -> f32| {
        c.shade(
            &id.shifted(u * 0.6, u * 0.8),
            all,
            sil,
            |_| hex(SHADOW).alpha(0.40),
            u * 0.35,
        );
    };

    // 水笔：横放在上方，笔尖朝左
    let (gy0, gy1) = (sn(0.1875), sn(0.3125));
    let (gx0, gx1) = (sn(0.19), sn(0.66));
    let gh = (gy1 - gy0) * 0.5;
    let gy = (gy0 + gy1) * 0.5;
    let pen_tip = move |p: V2| {
        poly(
            p,
            &[
                v(gx0 - u * 1.8, gy),
                v(gx0 + 1.0, gy0 + gh * 0.15),
                v(gx0 + 1.0, gy1 - gh * 0.15),
            ],
        )
    };
    let pen_body = move |p: V2| rbox(p, (gx0 + gx1) * 0.5, gy, (gx1 - gx0) * 0.5, gh, gh * 0.4);
    shadow(c, &|p| pen_tip(p).min(pen_body(p)));
    c.fill(&id, all, pen_tip, hex(0xaeb5c0));
    c.fill(&id, all, |p| pen_tip(p).max(p.x - (gx0 - u * 1.0)), hex(0x4a505a));
    c.fill(&id, all, pen_body, hex(0x2c45b4));
    c.fill(&id, all, |p| pen_body(p).max(p.x - (gx0 + u * 1.5)), hex(0x1b2668));
    c.fill(&id, all, |p| pen_body(p).max(gx1 - u * 1.5 - p.x), hex(0x1b2668));

    // 马克笔：竖放在右侧，笔头朝下
    let (mx0, mx1) = (sn(0.6875), sn(0.875));
    let (my0, my1) = (sn(0.19), sn(0.80));
    let mh = (mx1 - mx0) * 0.5;
    let mx = (mx0 + mx1) * 0.5;
    let m_body = move |p: V2| rbox(p, mx, (my0 + my1) * 0.5, mh, (my1 - my0) * 0.5, mh * 0.35);
    let m_nib = move |p: V2| rbox(p, mx, my1 + u * 0.75, mh * 0.55, u * 0.9, u * 0.2);
    shadow(c, &|p| m_body(p).min(m_nib(p)));
    c.fill(&id, all, m_nib, hex(0xe14b35));
    c.fill(
        &id,
        all,
        |p| rbox(p, mx, my1 - u * 0.5, mh, u * 0.5, 0.0).max(m_body(p)),
        hex(0xf1eee9),
    );
    c.fill(&id, all, |p| m_body(p).max(p.y - (my1 - u)), hex(0xf2644a));
    c.fill(&id, all, |p| m_body(p).max(p.y - (my0 + u * 2.5)), hex(0xb83a2a));

    // 油漆刷：45° 斜放，刷毛在左下，木柄伸向右上
    let xf = Xf::new(sn(0.44), sn(0.60), -45.0);
    let bb = [-360.0, -150.0, 360.0, 150.0];
    let w = 0.235 * ART * 0.5; // 刷毛和金属箍的半宽
    let hw = 0.11 * ART * 0.5; // 木柄半宽
    let bristles = move |p: V2| bar(p, -330.0, -150.0, w, w * 0.25);
    let ferrule = move |p: V2| bar(p, -165.0, -40.0, w * 1.04, w * 0.15);
    let handle = move |p: V2| capsule(p, v(-60.0, 0.0), v(300.0, 0.0), hw);
    let sil = move |p: V2| bristles(p).min(ferrule(p)).min(handle(p));
    c.shade(
        &xf.shifted(u * 0.6, u * 0.8),
        bb,
        sil,
        |_| hex(SHADOW).alpha(0.40),
        u * 0.35,
    );
    c.fill(&xf, bb, handle, hex(0xd99a5b));
    c.fill(&xf, bb, |p| handle(p).max(below(p, hw * 0.2)), hex(0xb97a3e));
    c.fill(&xf, bb, bristles, hex(0xe8d2a4));
    c.fill(&xf, bb, |p| bristles(p).max(p.x + 230.0), hex(0x16a594));
    c.fill(
        &xf,
        bb,
        |p| bristles(p).max(p.x + 230.0).max(below(p, w * 0.3)),
        hex(0x0f8273),
    );
    c.fill(&xf, bb, ferrule, hex(0xb4bcc8));
    c.fill(&xf, bb, |p| ferrule(p).max(below(p, w * 0.3)), hex(0x7f8999));
}
