// SPDX-License-Identifier: GPL-3.0-or-later
//! 界面的布局和绘制。固定布局：顶上一条快捷栏，左边两列面板，其余全是画布。
//!
//! 每次需要时由应用状态重新排一遍（`build`），得到一组带矩形的部件；
//! 绘制和点击判定都照这组部件来，不保存任何界面自己的状态。

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, FrameRect, GdiFlush, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, DRAW_TEXT_FORMAT, HDC, HFONT, TRANSPARENT,
};

use crate::xform::Mode;
use zhibi_core::filter::Adjust;
use zhibi_core::liquify::Op;
use crate::{App, Cmd, Tool, MAX_GAP, MAX_SWATCHES, SIZES};
use zhibi_core::lang::{english, tr};
use zhibi_core::trf;

const PANEL: u32 = 0xF4F4F2;
const BAR: u32 = 0xE9E9E6;
const LINE: u32 = 0xC9C9C4;
const TEXT: u32 = 0x25282C;
const DIM: u32 = 0x9A9DA1;
const ACCENT: u32 = 0x2F6FDE;
const ACCENT_SOFT: u32 = 0xD8E5FB;
const FIELD: u32 = 0xFFFFFF;
const TRACK: u32 = 0xD3D7DD;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SliderId {
    Size,
    MinSize,
    Density,
    MinDensity,
    Hardness,
    LayerOpacity,
    /// 油漆桶和魔棒的颜色容差
    Tolerance,
    /// 油漆桶和魔棒的缝隙闭合
    Gap,
    /// 液化笔刷的大小、强度
    LiquifySize,
    LiquifyStrength,
    /// 打开着的滤镜的第几个滑块
    Filter(u8),
    /// 前景色的红、绿、蓝（0、1、2）
    Rgb(u8),
    /// 前景色的色相、饱和度、明度（0、1、2）
    Hsv(u8),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Popup {
    Stabilizer,
    Blend,
}

pub enum Kind {
    Button { label: String, cmd: Cmd, on: bool, enabled: bool },
    Menu { label: String, which: Popup },
    Slider { label: &'static str, value: f32, text: String, id: SliderId },
    Wheel,
    /// 前景色或背景色的色块
    Swatch { fg: bool },
    /// 色板里存着的一个颜色
    Color { rgb: [u8; 3], on: bool },
    SizeCell { size: f32, on: bool },
    LayerRow { index: usize },
    Label(String),
}

pub struct Item {
    pub rect: RECT,
    pub kind: Kind,
}

pub struct Layout {
    pub top: RECT,
    pub left: RECT,
    pub layers: RECT,
    pub canvas: RECT,
    pub items: Vec<Item>,
}

pub fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 滤镜的各个滑块：名字、滑块位置（0 到 1）、显示的数值。
pub fn filter_sliders(a: &Adjust) -> Vec<(&'static str, f32, String)> {
    // 有正有负的参数，滑块正中是零。
    let mid = |v: f32| (v + 1.0) / 2.0;
    let signed = |v: f32| format!("{:+.0}", v * 100.0);
    match *a {
        Adjust::HueSat { hue, saturation, lightness } => vec![
            (tr("色相", "Hue"), mid(hue / 180.0), format!("{hue:+.0}°")),
            (tr("饱和度", "Saturation"), mid(saturation), signed(saturation)),
            (tr("明度", "Lightness"), mid(lightness), signed(lightness)),
        ],
        Adjust::BrightContrast { brightness, contrast } => {
            vec![(tr("亮度", "Brightness"), mid(brightness), signed(brightness)), (tr("对比度", "Contrast"), mid(contrast), signed(contrast))]
        }
        // 半径按平方根铺在滑块上：小半径那一头分得细。
        Adjust::Blur { radius } => vec![(tr("半径", "Radius"), (radius / 100.0).sqrt(), trf!("{} 像素", "{} px", radius))],
    }
}

/// 第 `i` 个滑块拖到 `t`（0 到 1）之后的参数。数值取整到滑块上显示的那一位。
pub fn set_filter_param(a: Adjust, i: u8, t: f32) -> Adjust {
    // 加 0.0 是为了把 −0 变成 0，免得显示成"−0"。
    let signed = |t: f32| ((t * 2.0 - 1.0) * 100.0).round() / 100.0 + 0.0;
    match a {
        Adjust::HueSat { hue, saturation, lightness } => match i {
            0 => Adjust::HueSat { hue: ((t * 2.0 - 1.0) * 180.0).round() + 0.0, saturation, lightness },
            1 => Adjust::HueSat { hue, saturation: signed(t), lightness },
            _ => Adjust::HueSat { hue, saturation, lightness: signed(t) },
        },
        Adjust::BrightContrast { brightness, contrast } => match i {
            0 => Adjust::BrightContrast { brightness: signed(t), contrast },
            _ => Adjust::BrightContrast { brightness, contrast: signed(t) },
        },
        Adjust::Blur { .. } => {
            let r = 100.0 * t * t;
            Adjust::Blur { radius: if r < 10.0 { (r * 10.0).round() / 10.0 } else { r.round() } }
        }
    }
}

/// 笔刷大小与滑块位置的换算：对数刻度，小笔刷那一头分得细。
pub fn size_to_slider(size: f32) -> f32 {
    (size.max(1.0).ln() / 500f32.ln()).clamp(0.0, 1.0)
}

pub fn slider_to_size(t: f32) -> f32 {
    let s = (t.clamp(0.0, 1.0) * 500f32.ln()).exp();
    if s < 10.0 {
        (s * 2.0).round() / 2.0
    } else {
        s.round()
    }
}

pub fn make_font(scale: f32) -> HFONT {
    unsafe {
        CreateFontW(
            -(13.0 * scale) as i32,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            Default::default(),
            Default::default(),
            CLEARTYPE_QUALITY,
            0,
            windows::core::w!("Microsoft YaHei UI"),
        )
    }
}

pub fn build(app: &App, client: (i32, i32)) -> Layout {
    let s = app.scale;
    let px = |v: f32| (v * s).round() as i32;
    let (cw, ch) = client;
    let top = RECT { left: 0, top: 0, right: cw, bottom: px(38.0) };
    let (lw, yw) = if app.panels { (px(264.0), px(232.0)) } else { (0, 0) };
    let left = RECT { left: 0, top: top.bottom, right: lw, bottom: ch };
    let layers = RECT { left: lw, top: top.bottom, right: lw + yw, bottom: ch };
    // 面板可以换到右边（规格 F-154）：先照在左边的样子排，最后整组挪过去。
    let aside = if app.panels && app.panels_right { (cw - lw - yw).max(1) } else { 0 };
    let canvas = if aside > 0 {
        RECT { left: 0, top: top.bottom, right: aside, bottom: ch.max(top.bottom + 1) }
    } else {
        RECT { left: lw + yw, top: top.bottom, right: cw.max(lw + yw + 1), bottom: ch.max(top.bottom + 1) }
    };
    let mut items = Vec::new();

    // ---- 快捷栏 ----
    let pad = px(6.0);
    let mut x = pad;
    let bar_h = RECT { left: 0, top: top.top + px(5.0), right: 0, bottom: top.bottom - px(5.0) };
    let mut bar = |items: &mut Vec<Item>, label: String, cmd: Cmd, on: bool, enabled: bool, w: f32, gap: f32| {
        let r = RECT { left: x, right: x + px(w), ..bar_h };
        x = r.right + px(gap);
        items.push(Item { rect: r, kind: Kind::Button { label, cmd, on, enabled } });
    };
    // 英文的字比中文长，按钮按语言给不同的宽度。
    let w = |zh: f32, en: f32| if english() { en } else { zh };
    bar(&mut items, tr("撤销", "Undo").into(), Cmd::Undo, false, app.can_undo(), w(52.0, 56.0), 2.0);
    bar(&mut items, tr("重做", "Redo").into(), Cmd::Redo, false, app.can_redo(), w(52.0, 56.0), 18.0);
    bar(&mut items, tr("缩小", "Zoom −").into(), Cmd::ZoomOut, false, true, w(48.0, 64.0), 2.0);
    bar(&mut items, format!("{:.0}%", app.view.zoom * 100.0), Cmd::Zoom100, false, true, 64.0, 2.0);
    bar(&mut items, tr("放大", "Zoom +").into(), Cmd::ZoomIn, false, true, w(48.0, 64.0), 2.0);
    bar(&mut items, tr("全图", "Fit").into(), Cmd::DefaultView, false, true, 48.0, 18.0);
    bar(&mut items, tr("左转", "Rotate L").into(), Cmd::RotateCcw, false, true, w(48.0, 72.0), 2.0);
    bar(&mut items, format!("{:.1}°", app.view.angle.to_degrees()), Cmd::AngleZero, false, true, 64.0, 2.0);
    bar(&mut items, tr("右转", "Rotate R").into(), Cmd::RotateCw, false, true, w(48.0, 72.0), 18.0);
    // 翻转期间这个按钮一直亮着，免得忘了自己在镜像里（规格 F-106）。
    bar(&mut items, tr("镜像", "Mirror").into(), Cmd::Flip, app.view.flip, true, w(52.0, 60.0), 18.0);
    let r = RECT { left: x, right: x + px(w(112.0, 130.0)), ..bar_h };
    items.push(Item { rect: r, kind: Kind::Menu { label: trf!("稳定器 {}", "Stabilizer {}", app.stabilizer), which: Popup::Stabilizer } });

    if !app.panels {
        return Layout { top, left, layers, canvas, items };
    }

    // ---- 左列：颜色、工具、笔刷参数、大小表 ----
    let panel_items = items.len();
    let m = px(10.0);
    let pct = |v: f32| format!("{:.0}%", v * 100.0);
    let label = |items: &mut Vec<Item>, y: &mut i32, text: String| {
        items.push(Item { rect: RECT { left: m, top: *y, right: lw - m, bottom: *y + px(20.0) }, kind: Kind::Label(text) });
        *y += px(22.0);
    };
    let row = |items: &mut Vec<Item>, y: &mut i32, buttons: &[(&str, Cmd, bool, bool)]| {
        let n = buttons.len() as i32;
        let bw = (lw - 2 * m - (n - 1) * px(4.0)) / n;
        for (i, (text, cmd, on, enabled)) in buttons.iter().enumerate() {
            let l = m + i as i32 * (bw + px(4.0));
            items.push(Item {
                rect: RECT { left: l, top: *y, right: l + bw, bottom: *y + px(30.0) },
                kind: Kind::Button { label: (*text).into(), cmd: *cmd, on: *on, enabled: *enabled },
            });
        }
        *y += px(34.0);
    };
    let slider = |items: &mut Vec<Item>, y: &mut i32, label: &'static str, value: f32, text: String, id: SliderId| {
        items.push(Item { rect: RECT { left: m, top: *y, right: lw - m, bottom: *y + px(24.0) }, kind: Kind::Slider { label, value, text, id } });
        *y += px(29.0);
    };

    let mut y = left.top + m;
    let wheel = (lw - 2 * m).min(px(232.0));
    items.push(Item { rect: RECT { left: (lw - wheel) / 2, top: y, right: (lw + wheel) / 2, bottom: y + wheel }, kind: Kind::Wheel });
    y += wheel + px(8.0);
    let sw = px(44.0);
    items.push(Item { rect: RECT { left: m, top: y, right: m + sw, bottom: y + px(28.0) }, kind: Kind::Swatch { fg: true } });
    items.push(Item { rect: RECT { left: m + sw + px(4.0), top: y, right: m + 2 * sw + px(4.0), bottom: y + px(28.0) }, kind: Kind::Swatch { fg: false } });
    items.push(Item {
        rect: RECT { left: m + 2 * sw + px(12.0), top: y, right: m + 2 * sw + px(64.0), bottom: y + px(28.0) },
        kind: Kind::Button { label: tr("交换", "Swap").into(), cmd: Cmd::SwapColors, on: false, enabled: true },
    });
    y += px(38.0);

    // 其余的颜色面板出厂都不显示，在"视图"菜单里各自开关（规格 F-093）。
    let color = app.color();
    if app.show_rgb {
        for (i, name) in [tr("红", "Red"), tr("绿", "Green"), tr("蓝", "Blue")].into_iter().enumerate() {
            slider(&mut items, &mut y, name, color[i] as f32 / 255.0, format!("{}", color[i]), SliderId::Rgb(i as u8));
        }
        y += px(6.0);
    }
    if app.show_hsv {
        let (h, s, v) = app.hsv;
        slider(&mut items, &mut y, tr("色相", "Hue"), h / 360.0, format!("{h:.0}°"), SliderId::Hsv(0));
        slider(&mut items, &mut y, tr("饱和度", "Saturation"), s, pct(s), SliderId::Hsv(1));
        slider(&mut items, &mut y, tr("明度", "Value"), v, pct(v), SliderId::Hsv(2));
        y += px(6.0);
    }
    if app.show_swatches {
        let cols = 8;
        let cell = (lw - 2 * m) / cols;
        for (i, rgb) in app.swatches.iter().enumerate() {
            let (cx, cy) = (i as i32 % cols, i as i32 / cols);
            let r = RECT { left: m + cx * cell, top: y + cy * cell, right: m + (cx + 1) * cell - px(2.0), bottom: y + (cy + 1) * cell - px(2.0) };
            items.push(Item { rect: r, kind: Kind::Color { rgb: *rgb, on: *rgb == color } });
        }
        y += (app.swatches.len() as i32 + cols - 1) / cols * cell + px(4.0);
        let stored = app.swatches.contains(&color);
        row(&mut items, &mut y, &[(tr("存下当前颜色", "Save Color"), Cmd::SwatchAdd, false, !stored && app.swatches.len() < MAX_SWATCHES), (tr("删掉当前颜色", "Remove Color"), Cmd::SwatchRemove, false, stored)]);
        y += px(6.0);
    }

    // 工具：两行，上面一行是画的（五个），下面一行是选和挪的。
    let cols = if english() { 4 } else { 5 };
    let tw = (lw - 2 * m - (cols - 1) * px(4.0)) / cols;
    for (i, tool) in Tool::ALL.into_iter().enumerate() {
        let l = m + (i as i32 % cols) * (tw + px(4.0));
        let t = y + (i as i32 / cols) * px(34.0);
        items.push(Item {
            rect: RECT { left: l, top: t, right: l + tw, bottom: t + px(30.0) },
            kind: Kind::Button { label: tool.label().into(), cmd: Cmd::SelectTool(tool), on: app.tool == tool, enabled: true },
        });
    }
    let rows = (Tool::ALL.len() as i32 + cols - 1) / cols;
    y += rows * px(34.0) + px(8.0);

    // 工具下面这一块跟着当前工具换内容；变换进行中时换成变换的选项和确定、取消（规格 F-080）。
    let selected = app.doc.selection().is_some();

    if let Some(x) = app.xform_shown() {
        label(&mut items, &mut y, tr("自由变换", "Free Transform").into());
        label(&mut items, &mut y, trf!("宽 {:.0}%　高 {:.0}%　转 {:.1}°", "W {:.0}%  H {:.0}%  {:.1}°", x.scale.0.abs() * 100.0, x.scale.1.abs() * 100.0, x.angle.to_degrees()));
        // 宽、高、角度说的只是外框的位置；做过扭曲或网格变形的，另起一行注明。
        let extra = match (x.warped(), x.bent()) {
            (true, true) => Some(tr("另有扭曲和网格变形", "Plus distortion and warp")),
            (true, false) => Some(tr("另有扭曲", "Plus distortion")),
            (false, true) => Some(tr("另有网格变形", "Plus warp")),
            (false, false) => None,
        };
        if let Some(text) = extra {
            label(&mut items, &mut y, text.into());
        }
        y += px(4.0);
        row(&mut items, &mut y, &[(tr("确定  Enter", "OK  Enter"), Cmd::XformOk, true, true), (tr("取消  Esc", "Cancel  Esc"), Cmd::XformCancel, false, true)]);
        y += px(6.0);
        let modes = [(tr("自由", "Free"), Mode::Free), (tr("扭曲", "Distort"), Mode::Distort), (tr("网格", "Warp"), Mode::Mesh)];
        row(&mut items, &mut y, &modes.map(|(text, mode)| (text, Cmd::XformMode(mode), x.mode == mode, true)));
        y += px(6.0);
        row(&mut items, &mut y, &[(tr("左右翻转", "Flip Horizontal"), Cmd::FlipH, false, true), (tr("上下翻转", "Flip Vertical"), Cmd::FlipV, false, true)]);
        row(&mut items, &mut y, &[(tr("左转 90°", "Rotate 90° Left"), Cmd::TurnCcw, false, true), (tr("右转 90°", "Rotate 90° Right"), Cmd::TurnCw, false, true)]);
        row(&mut items, &mut y, &[(tr("复位", "Reset"), Cmd::XformReset, false, true)]);
        y += px(6.0);
        let hints: &[&str] = match x.mode {
            Mode::Free => &[
                tr("拖角：缩放（按住 Shift 可以变形）", "Corner: scale (Shift: free)"),
                tr("拖边：只拉一个方向", "Edge: stretch one way"),
                tr("按住 Ctrl 拖角、拖边：扭曲、斜切", "Ctrl+drag: distort, skew"),
            ],
            Mode::Distort => &[tr("拖角：只动这一个角", "Corner: move this corner only"), tr("拖边：斜切", "Edge: skew")],
            Mode::Mesh => &[tr("拖网格上的点：画面跟着弯", "Drag a point to bend the picture")],
        };
        for hint in hints.iter().chain(&[tr("框内拖动：移动", "Inside: move"), tr("框外拖动：旋转（Shift 每档 15°）", "Outside: rotate (Shift: 15°)")]) {
            label(&mut items, &mut y, (*hint).into());
        }
    } else if let Some(f) = app.filter.as_ref() {
        // 滤镜打开着：面板换成它的滑块和确定、取消（规格 F-095），不弹对话框。
        label(&mut items, &mut y, f.kind.label().into());
        for (i, (name, value, text)) in filter_sliders(&f.params).into_iter().enumerate() {
            slider(&mut items, &mut y, name, value, text, SliderId::Filter(i as u8));
        }
        y += px(4.0);
        row(&mut items, &mut y, &[(tr("确定  Enter", "OK  Enter"), Cmd::FilterOk, true, true), (tr("取消  Esc", "Cancel  Esc"), Cmd::FilterCancel, false, true)]);
        row(&mut items, &mut y, &[(tr("复位", "Reset"), Cmd::FilterReset, false, true)]);
        y += px(6.0);
        for hint in [tr("拖滑块，画布跟着变", "The canvas follows the sliders"), tr("只改当前图层", "Changes the current layer only"), tr("有选区时只改选区里", "With a selection: only inside it")] {
            label(&mut items, &mut y, hint.into());
        }
    } else if app.tool.is_brush() {
        let b = app.brush();
        slider(&mut items, &mut y, tr("大小", "Size"), size_to_slider(b.size), format!("{}", b.size), SliderId::Size);
        slider(&mut items, &mut y, tr("最小大小", "Min Size"), b.min_size, pct(b.min_size), SliderId::MinSize);
        slider(&mut items, &mut y, tr("浓度", "Density"), b.density, pct(b.density), SliderId::Density);
        slider(&mut items, &mut y, tr("最小浓度", "Min Density"), b.min_density, pct(b.min_density), SliderId::MinDensity);
        slider(&mut items, &mut y, tr("边缘硬度", "Hardness"), b.hardness, pct(b.hardness), SliderId::Hardness);
        if app.tool == Tool::Marker {
            label(&mut items, &mut y, tr("叠涂不会变深", "Overlaps don\u{2019}t get darker").into());
            label(&mut items, &mut y, tr("轻压时向新颜色过渡", "Light pressure blends colors").into());
            label(&mut items, &mut y, tr("边缘糊不糊用\"边缘硬度\"调，100% 最清楚", "Edge blur: Hardness, 100% is crisp").into());
        }
        y += px(6.0);
        let cols = 5;
        let cell = (lw - 2 * m) / cols;
        let cell_h = px(26.0);
        for (i, &size) in SIZES.iter().enumerate() {
            let (cx, cy) = (i as i32 % cols, i as i32 / cols);
            let r = RECT { left: m + cx * cell, top: y + cy * cell_h, right: m + (cx + 1) * cell, bottom: y + (cy + 1) * cell_h };
            if r.bottom > left.bottom - m {
                break;
            }
            items.push(Item { rect: r, kind: Kind::SizeCell { size, on: (b.size - size).abs() < 0.01 } });
        }
    } else {
        match app.tool {
            Tool::Bucket | Tool::Wand => {
                slider(&mut items, &mut y, tr("颜色容差", "Tolerance"), app.tolerance as f32 / 255.0, format!("{}", app.tolerance), SliderId::Tolerance);
                let gap = if app.gap == 0 { tr("关", "Off").to_string() } else { trf!("{} 像素", "{} px", app.gap) };
                slider(&mut items, &mut y, tr("缝隙闭合", "Gap Closing"), app.gap as f32 / MAX_GAP as f32, gap, SliderId::Gap);
                y += px(6.0);
                label(&mut items, &mut y, tr("看哪里的颜色", "Sample from").into());
                row(&mut items, &mut y, &[(tr("所有图层", "All Layers"), Cmd::FillSource(true), app.fill_merged, true), (tr("当前图层", "This Layer"), Cmd::FillSource(false), !app.fill_merged, true)]);
                y += px(6.0);
                if app.tool == Tool::Bucket {
                    label(&mut items, &mut y, tr("点一下：用前景色填满这一片", "Click: fill this area").into());
                    label(&mut items, &mut y, tr("线没画闭合时，把缝隙闭合调大", "Leaking? Raise Gap Closing").into());
                } else {
                    label(&mut items, &mut y, tr("点一下：选中颜色相近的一片", "Click: select similar colors").into());
                    label(&mut items, &mut y, tr("Shift 加选，Alt 减选", "Shift: add, Alt: subtract").into());
                }
            }
            Tool::Liquify => {
                // 六种方式两行，每行三个（规格 F-083）。
                let ops = [
                    (tr("推", "Push"), Op::Push),
                    (tr("顺时针转", "Twirl CW"), Op::TwirlCw),
                    (tr("逆时针转", "Twirl CCW"), Op::TwirlCcw),
                    (tr("收缩", "Pinch"), Op::Pinch),
                    (tr("膨胀", "Bloat"), Op::Bloat),
                    (tr("复原", "Restore"), Op::Restore),
                ]
                .map(|(text, op)| (text, Cmd::LiquifyOp(op), app.liquid.op == op, true));
                row(&mut items, &mut y, &ops[..3]);
                row(&mut items, &mut y, &ops[3..]);
                y += px(4.0);
                let b = app.liquid;
                slider(&mut items, &mut y, tr("大小", "Size"), size_to_slider(b.size), format!("{}", b.size), SliderId::LiquifySize);
                slider(&mut items, &mut y, tr("强度", "Strength"), b.strength, pct(b.strength), SliderId::LiquifyStrength);
                y += px(6.0);
                let on = app.liquifying();
                row(&mut items, &mut y, &[(tr("确定  Enter", "OK  Enter"), Cmd::LiquifyOk, on, on), (tr("取消  Esc", "Cancel  Esc"), Cmd::LiquifyCancel, false, on)]);
                y += px(6.0);
                let how = match b.op {
                    Op::Push => tr("拖动：把像素推着走", "Drag: push pixels along"),
                    Op::TwirlCw | Op::TwirlCcw => tr("按住：绕笔刷中心转，越久转得越多", "Hold: twirl, longer turns more"),
                    Op::Pinch => tr("按住：往笔刷中心收", "Hold: pull toward the center"),
                    Op::Bloat => tr("按住：从笔刷中心往外鼓", "Hold: push out from the center"),
                    Op::Restore => tr("涂过的地方回到液化之前", "Paint back the original"),
                };
                for hint in [how, tr("压得越重越强", "Press harder for more"), tr("确定之前，每一笔都能撤销", "Each stroke can be undone"), tr("换工具、做别的操作时自动确定", "Other tools apply it")] {
                    label(&mut items, &mut y, hint.into());
                }
            }
            Tool::Select | Tool::Lasso => {
                label(&mut items, &mut y, if app.tool == Tool::Select { tr("拖动：框出一块矩形", "Drag: select a rectangle") } else { tr("拖动：圈出一块", "Drag: draw around an area") }.into());
                label(&mut items, &mut y, tr("Shift 加选，Alt 减选", "Shift: add, Alt: subtract").into());
                label(&mut items, &mut y, tr("有选区时，画、擦、填都只在选区里", "Painting stays inside a selection").into());
            }
            _ => {
                label(&mut items, &mut y, tr("拖动：移动当前图层", "Drag: move the layer").into());
                label(&mut items, &mut y, tr("有选区时只移动选中的部分", "With a selection: only that part").into());
                label(&mut items, &mut y, tr("用别的工具时按住 Ctrl 拖动也一样", "Ctrl+drag works with any tool").into());
                y += px(6.0);
                row(&mut items, &mut y, &[(tr("自由变换  Ctrl+T", "Free Transform  Ctrl+T"), Cmd::Transform, false, true)]);
            }
        }
        if app.tool != Tool::Bucket {
            y += px(6.0);
            row(&mut items, &mut y, &[(tr("全选", "All"), Cmd::SelectAll, false, true), (tr("取消", "None"), Cmd::Deselect, false, selected), (tr("反选", "Invert"), Cmd::InvertSelection, false, true)]);
        }
    }

    // ---- 第二列：图层 ----
    let (l, r) = (layers.left + m, layers.right - m);
    let mut y = layers.top + m;
    items.push(Item { rect: RECT { left: l, top: y, right: r, bottom: y + px(20.0) }, kind: Kind::Label(tr("图层", "Layers").into()) });
    y += px(26.0);
    let active = &app.doc.layers()[app.doc.active()].props;
    items.push(Item {
        rect: RECT { left: l, top: y, right: r, bottom: y + px(28.0) },
        kind: Kind::Menu { label: trf!("混合：{}", "Blend: {}", active.blend.label()), which: Popup::Blend },
    });
    y += px(34.0);
    items.push(Item {
        rect: RECT { left: l, top: y, right: r, bottom: y + px(24.0) },
        kind: Kind::Slider { label: tr("不透明度", "Opacity"), value: active.opacity, text: pct(active.opacity), id: SliderId::LayerOpacity },
    });
    y += px(30.0);
    // 当前图层的三个开关：保护不透明度、锁定、剪贴（规格 F-053、F-054）。
    let toggles = [(tr("保护透明", "Lock α"), Cmd::ToggleLockAlpha, active.lock_alpha), (tr("锁定", "Lock"), Cmd::ToggleLocked, active.locked), (tr("剪贴", "Clip"), Cmd::ToggleClip, active.clip)];
    let tw = (r - l - 2 * px(4.0)) / 3;
    for (i, (label, cmd, on)) in toggles.into_iter().enumerate() {
        let tl = l + i as i32 * (tw + px(4.0));
        items.push(Item {
            rect: RECT { left: tl, top: y, right: tl + tw, bottom: y + px(26.0) },
            kind: Kind::Button { label: label.into(), cmd, on, enabled: true },
        });
    }
    y += px(34.0);
    let n = app.doc.layers().len();
    let a = app.doc.active();
    let rows = [
        [(tr("新建", "New"), Cmd::LayerNew, true), (tr("复制", "Copy"), Cmd::LayerDup, true), (tr("删除", "Delete"), Cmd::LayerDel, n > 1)],
        [(tr("上移", "Up"), Cmd::LayerUp, a + 1 < n), (tr("下移", "Down"), Cmd::LayerDown, a > 0), (tr("向下合并", "Merge"), Cmd::LayerMerge, a > 0)],
    ];
    let bw = (r - l - 2 * px(4.0)) / 3;
    for row in rows {
        for (i, (label, cmd, enabled)) in row.into_iter().enumerate() {
            let bl = l + i as i32 * (bw + px(4.0));
            items.push(Item {
                rect: RECT { left: bl, top: y, right: bl + bw, bottom: y + px(28.0) },
                kind: Kind::Button { label: label.into(), cmd, on: false, enabled },
            });
        }
        y += px(32.0);
    }
    y += px(4.0);
    // 列表里最上面一行是叠在最上面的图层。
    for index in (0..n).rev() {
        let rect = RECT { left: l, top: y, right: r, bottom: y + px(40.0) };
        if rect.bottom > layers.bottom - m {
            break;
        }
        items.push(Item { rect, kind: Kind::LayerRow { index } });
        y += px(42.0);
    }

    let (mut left, mut layers) = (left, layers);
    if aside > 0 {
        for item in &mut items[panel_items..] {
            item.rect.left += aside;
            item.rect.right += aside;
        }
        for r in [&mut left, &mut layers] {
            r.left += aside;
            r.right += aside;
        }
    }
    Layout { top, left, layers, canvas, items }
}

/// 图层行里"眼睛"开关的位置。
pub fn eye_rect(row: &RECT, scale: f32) -> RECT {
    let s = (26.0 * scale) as i32;
    let pad = (row.bottom - row.top - s) / 2;
    RECT { left: row.left + pad, top: row.top + pad, right: row.left + pad + s, bottom: row.top + pad + s }
}

/// 图层行里名称的位置（原地改名的输入框也放在这里）。
pub fn name_rect(row: &RECT, scale: f32) -> RECT {
    let e = eye_rect(row, scale);
    RECT { left: e.right + (8.0 * scale) as i32, top: row.top + (6.0 * scale) as i32, right: row.right - (6.0 * scale) as i32, bottom: row.bottom - (6.0 * scale) as i32 }
}

fn rgb(c: u32) -> COLORREF {
    COLORREF(((c >> 16) & 0xFF) | (c & 0xFF00) | ((c & 0xFF) << 16))
}

unsafe fn fill(dc: HDC, r: &RECT, c: u32) {
    let b = CreateSolidBrush(rgb(c));
    FillRect(dc, r, b);
    let _ = DeleteObject(b.into());
}

unsafe fn frame(dc: HDC, r: &RECT, c: u32) {
    let b = CreateSolidBrush(rgb(c));
    FrameRect(dc, r, b);
    let _ = DeleteObject(b.into());
}

unsafe fn text(dc: HDC, r: &RECT, s: &str, c: u32, align: DRAW_TEXT_FORMAT) {
    let mut w: Vec<u16> = s.encode_utf16().collect();
    let mut r = *r;
    SetTextColor(dc, rgb(c));
    DrawTextW(dc, &mut w, &mut r, align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
}

/// HSV（h 为 0..360，s、v 为 0..1）转 RGB。
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let (i, f) = (h.floor(), h.fract());
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    let (r, g, b) = match i as i32 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [(r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8]
}

pub fn rgb_to_hsv(c: [u8; 3]) -> (f32, f32, f32) {
    let (r, g, b) = (c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, if max == 0.0 { 0.0 } else { d / max }, max)
}

/// 色环的几何：外半径、内半径、中间方块的半边长（都相对色环矩形的中心）。
pub fn wheel_geometry(r: &RECT) -> ((f32, f32), f32, f32, f32) {
    let c = ((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0);
    let outer = (r.right - r.left) as f32 / 2.0 - 1.0;
    let inner = outer * 0.82;
    (c, outer, inner, inner * 0.68)
}

/// 色环：外圈选色相，中间方块横向是饱和度、纵向是明度（规格 F-090）。直接写进像素缓冲。
fn draw_wheel(bits: &mut [u32], stride: usize, r: &RECT, hue: f32, sat: f32, val: f32) {
    let (c, outer, inner, half) = wheel_geometry(r);
    for y in r.top..r.bottom {
        for x in r.left..r.right {
            let (dx, dy) = (x as f32 + 0.5 - c.0, y as f32 + 0.5 - c.1);
            let d = (dx * dx + dy * dy).sqrt();
            let px = &mut bits[y as usize * stride + x as usize];
            if dx.abs() <= half && dy.abs() <= half {
                let col = hsv_to_rgb(hue, (dx + half) / (2.0 * half), 1.0 - (dy + half) / (2.0 * half));
                *px = ((col[0] as u32) << 16) | ((col[1] as u32) << 8) | col[2] as u32;
            } else if d >= inner - 1.0 && d <= outer + 1.0 {
                let col = hsv_to_rgb(dy.atan2(dx).to_degrees(), 1.0, 1.0);
                // 环的内外边缘各留一个像素的过渡，免得边上有锯齿。
                let a = ((d - (inner - 1.0)).min(outer + 1.0 - d)).clamp(0.0, 1.0);
                let mix = |f: u8, b: u32| (f as f32 * a + b as f32 * (1.0 - a)) as u32;
                *px = (mix(col[0], (PANEL >> 16) & 0xFF) << 16) | (mix(col[1], (PANEL >> 8) & 0xFF) << 8) | mix(col[2], PANEL & 0xFF);
            }
        }
    }
    // 两个小圆圈标出当前的色相和饱和度/明度。
    let mut ring = |cx: f32, cy: f32, rad: f32, color: u32| {
        let steps = (rad * 8.0) as i32;
        for k in 0..steps {
            let a = k as f32 * std::f32::consts::TAU / steps as f32;
            for t in [0.0, 1.0] {
                let (x, y) = ((cx + (rad + t) * a.cos()).round() as i32, (cy + (rad + t) * a.sin()).round() as i32);
                if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
                    bits[y as usize * stride + x as usize] = if t == 0.0 { color } else { 0x0020_2020 };
                }
            }
        }
    };
    let mid = (inner + outer) / 2.0;
    let ha = hue.to_radians();
    ring(c.0 + mid * ha.cos(), c.1 + mid * ha.sin(), (outer - inner) / 2.0 - 2.0, 0x00FF_FFFF);
    ring(c.0 - half + sat * 2.0 * half, c.1 - half + (1.0 - val) * 2.0 * half, 5.0, 0x00FF_FFFF);
}

/// 把快捷栏和两列面板画进后台缓冲。`dc` 是选入了该缓冲的内存设备上下文。
pub fn paint(app: &App, layout: &Layout, dc: HDC, bits: &mut [u32], stride: usize) {
    unsafe {
        let old = SelectObject(dc, app.font.into());
        SetBkMode(dc, TRANSPARENT);
        fill(dc, &layout.top, BAR);
        fill(dc, &RECT { top: layout.top.bottom - 1, ..layout.top }, LINE);
        if app.panels {
            fill(dc, &layout.left, PANEL);
            fill(dc, &layout.layers, PANEL);
            fill(dc, &RECT { left: layout.left.right - 1, ..layout.left }, LINE);
            fill(dc, &RECT { left: layout.layers.right - 1, ..layout.layers }, LINE);
            if layout.left.left > 0 {
                fill(dc, &RECT { right: layout.left.left + 1, ..layout.left }, LINE);
            }
        }
        let center = DT_CENTER;
        for item in &layout.items {
            let r = &item.rect;
            match &item.kind {
                Kind::Button { label, on, enabled, .. } => {
                    fill(dc, r, if *on { ACCENT } else { FIELD });
                    frame(dc, r, if *on { ACCENT } else { LINE });
                    text(dc, r, label, if *on { 0xFFFFFF } else if *enabled { TEXT } else { DIM }, center);
                }
                Kind::Menu { label, .. } => {
                    fill(dc, r, FIELD);
                    frame(dc, r, LINE);
                    let inner = RECT { left: r.left + (8.0 * app.scale) as i32, right: r.right - (8.0 * app.scale) as i32, ..*r };
                    text(dc, &inner, label, TEXT, DT_LEFT);
                    text(dc, &inner, "▾", DIM, DT_RIGHT);
                }
                Kind::Slider { label, value, text: value_text, .. } => {
                    fill(dc, r, TRACK);
                    let w = ((r.right - r.left) as f32 * value.clamp(0.0, 1.0)) as i32;
                    fill(dc, &RECT { right: r.left + w, ..*r }, ACCENT_SOFT);
                    fill(dc, &RECT { left: (r.left + w - 2).max(r.left), right: (r.left + w).max(r.left + 2), ..*r }, ACCENT);
                    let inner = RECT { left: r.left + (8.0 * app.scale) as i32, right: r.right - (8.0 * app.scale) as i32, ..*r };
                    text(dc, &inner, label, TEXT, DT_LEFT);
                    text(dc, &inner, value_text, TEXT, DT_RIGHT);
                }
                Kind::Wheel => {}
                Kind::Swatch { fg } => {
                    let c = if *fg { app.color() } else { app.background };
                    fill(dc, r, ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32);
                    frame(dc, r, if *fg { TEXT } else { LINE });
                }
                Kind::Color { rgb, on } => {
                    fill(dc, r, ((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32);
                    frame(dc, r, if *on { TEXT } else { LINE });
                }
                Kind::SizeCell { size, on } => {
                    if *on {
                        fill(dc, r, ACCENT);
                    }
                    text(dc, r, &format!("{size}"), if *on { 0xFFFFFF } else { TEXT }, center);
                }
                Kind::LayerRow { index } => {
                    let layer = &app.doc.layers()[*index];
                    let on = *index == app.doc.active();
                    fill(dc, r, if on { ACCENT_SOFT } else { FIELD });
                    frame(dc, r, if on { ACCENT } else { LINE });
                    let eye = eye_rect(r, app.scale);
                    fill(dc, &eye, if layer.props.visible { TEXT } else { FIELD });
                    frame(dc, &eye, TEXT);
                    text(dc, &eye, if layer.props.visible { tr("显", "On") } else { tr("隐", "Off") }, if layer.props.visible { 0xFFFFFF } else { DIM }, center);
                    // 名字前面的箭头表示它剪贴在下面的图层上；后面跟着不透明度和锁的标记。
                    let p = &layer.props;
                    let mut label = format!("{}{}", if p.clip { "↳ " } else { "" }, p.name);
                    if p.opacity < 0.995 {
                        label += &format!("  {:.0}%", p.opacity * 100.0);
                    }
                    if p.locked {
                        label += tr("  〔锁〕", "  [locked]");
                    } else if p.lock_alpha {
                        label += tr("  〔护〕", "  [alpha]");
                    }
                    text(dc, &name_rect(r, app.scale), &label, if layer.props.visible { TEXT } else { DIM }, DT_LEFT);
                }
                Kind::Label(s) => text(dc, r, s, DIM, DT_LEFT),
            }
        }
        SelectObject(dc, old);
        // 上面的系统绘图调用可能还没落到缓冲里，直接写像素之前先让它们完成。
        let _ = GdiFlush();
    }
    if let Some(item) = layout.items.iter().find(|i| matches!(i.kind, Kind::Wheel)) {
        draw_wheel(bits, stride, &item.rect, app.hsv.0, app.hsv.1, app.hsv.2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC, GetTextExtentPoint32W};

    #[test]
    fn filter_sliders_map_both_ways() {
        let zero = [Adjust::HueSat { hue: 0.0, saturation: 0.0, lightness: 0.0 }, Adjust::BrightContrast { brightness: 0.0, contrast: 0.0 }, Adjust::Blur { radius: 0.0 }];
        for a in zero {
            let n = filter_sliders(&a).len();
            for i in 0..n as u8 {
                // 有正负的滑块正中是零（显示"+0"而不是"−0"），两头是上下限。
                let lo = set_filter_param(a, i, 0.0);
                let hi = set_filter_param(a, i, 1.0);
                if !matches!(a, Adjust::Blur { .. }) {
                    let mid = set_filter_param(a, i, 0.5);
                    assert!(mid.is_identity(), "{mid:?}");
                    assert!(filter_sliders(&mid).iter().all(|s| !s.2.starts_with('-')), "{mid:?}");
                }
                for (b, t) in [(lo, 0.0), (hi, 1.0)] {
                    let back = filter_sliders(&b)[i as usize].1;
                    assert!((back - t).abs() < 1e-3, "{b:?}: {back}");
                }
            }
        }
        // 半径：小的那一头分得细，取整到 0.1 像素；10 以上取整到 1 像素。
        assert_eq!(set_filter_param(zero[2], 0, 0.1), Adjust::Blur { radius: 1.0 });
        assert_eq!(set_filter_param(zero[2], 0, 0.5), Adjust::Blur { radius: 25.0 });
        assert_eq!(set_filter_param(zero[0], 0, 0.75), Adjust::HueSat { hue: 90.0, saturation: 0.0, lightness: 0.0 });
    }

    /// 英文界面里最紧的几处：字要放得下（按 100% 缩放量，宽度单位是逻辑像素，两边共留 4 像素）。
    /// 宽度照 `build` 里的算法：左列内容宽 244，工具按钮四列各 58，图层面板三列各 68，
    /// 左列三个一排各 78、两个一排各 120，稳定器下拉框扣掉边距和箭头后约 100。
    #[test]
    fn english_labels_fit_their_buttons() {
        let cases: &[(&[&str], i32)] = &[
            (&["Pen", "Marker", "Airbrush", "Eraser", "Bucket", "Select", "Lasso", "Wand", "Move", "Liquify"], 58),
            (&["Lock α", "Lock", "Clip", "New", "Copy", "Delete", "Up", "Down", "Merge"], 68),
            (&["All", "None", "Invert", "Free", "Distort", "Warp", "Push", "Twirl CW", "Twirl CCW", "Pinch", "Bloat", "Restore"], 78),
            (
                &[
                    "Flip Horizontal", "Flip Vertical", "Rotate 90° Left", "Rotate 90° Right", "Save Color", "Remove Color",
                    "All Layers", "This Layer", "OK  Enter", "Cancel  Esc",
                ],
                120,
            ),
            (
                &[
                    "Free Transform  Ctrl+T", "Corner: scale (Shift: free)", "Edge: stretch one way", "Outside: rotate (Shift: 15°)",
                    "Overlaps don\u{2019}t get darker", "Light pressure blends colors", "Edge blur: Hardness, 100% is crisp", "Click: fill this area",
                    "Leaking? Raise Gap Closing", "Click: select similar colors", "Shift: add, Alt: subtract", "Drag: select a rectangle",
                    "Drag: draw around an area", "Painting stays inside a selection", "Drag: move the layer",
                    "With a selection: only that part", "Ctrl+drag works with any tool", "W 100%  H 100%  -180.0°",
                    "Ctrl+drag: distort, skew", "Corner: move this corner only", "Edge: skew", "Drag a point to bend the picture",
                    "Plus distortion and warp", "Drag: push pixels along", "Hold: twirl, longer turns more", "Hold: pull toward the center",
                    "Hold: push out from the center", "Paint back the original", "Press harder for more", "Each stroke can be undone",
                    "Other tools apply it", "Brightness/Contrast", "The canvas follows the sliders", "Changes the current layer only",
                    "With a selection: only inside it",
                ],
                244,
            ),
            (&["Stabilizer 15"], 100),
            (&["Zoom −", "Zoom +"], 64),
            (&["Rotate L", "Rotate R"], 72),
            (&["Mirror"], 60),
            (&["Undo", "Redo"], 56),
        ];
        unsafe {
            let dc = CreateCompatibleDC(None);
            let font = make_font(1.0);
            let old = SelectObject(dc, font.into());
            for (labels, room) in cases {
                for label in labels.iter() {
                    let text: Vec<u16> = label.encode_utf16().collect();
                    let mut size = SIZE::default();
                    let _ = GetTextExtentPoint32W(dc, &text, &mut size);
                    assert!(size.cx + 4 <= *room, "\"{label}\" is {} px wide, room for {}", size.cx, room - 4);
                }
            }
            SelectObject(dc, old);
            let _ = DeleteObject(font.into());
            let _ = DeleteDC(dc);
        }
    }
}
