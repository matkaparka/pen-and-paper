// SPDX-License-Identifier: GPL-3.0-or-later
//! 纸笔：只做绘画的绘画软件。
//!
//! 这一层只做平台相关的事：窗口、笔和鼠标输入、界面、把画好的像素贴到屏幕上。
//! 画布、笔画、图层、撤销、文件格式都在 zhibi-core 里。

#![windows_subsystem = "windows"]
#![allow(non_snake_case)]

mod dialogs;
mod keys;
mod settings;
mod ui;
mod wintab;
mod xform;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, ClientToScreen, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint, GetDC,
    InvalidateRect, ReleaseDC, ScreenToClient, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    HBITMAP, HDC, HFONT, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::Ime::{ImmAssociateContext, HIMC};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_ESCAPE, VK_MENU, VK_RETURN, VK_SHIFT, VK_SPACE,
};
use windows::Win32::UI::Input::Pointer::{
    GetPointerDeviceRects, GetPointerPenInfoHistory, GetPointerType, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use keys::{Key, Keymap};
use ui::{Kind, Layout, Popup, SliderId};
use xform::{Handle, Xform};
use zhibi_core::document::{BlendMode, Document, LayerProps, MAX_SIDE};
use zhibi_core::fill::{self, FillOptions, Source};
use zhibi_core::selection::{Combine, Selection};
use zhibi_core::stabilizer::Sample;
use zhibi_core::stroke::{BrushParams, Stroke};
use zhibi_core::transform::Quality;
use zhibi_core::view::{Display, View};
use zhibi_core::lang::{self, tr};
use zhibi_core::{format, ora, psd, trf, Rect};

/// 笔刷大小预设表（规格 F-015）。方括号键在表里上下移动。
pub const SIZES: &[f32] = &[
    1.0, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 12.0, 14.0, 16.0, 20.0, 25.0, 30.0, 35.0, 40.0, 50.0, 60.0,
    70.0, 80.0, 100.0, 120.0, 160.0, 200.0, 250.0, 300.0, 400.0, 500.0,
];
/// 按键缩放的档位（规格 F-104）。
const ZOOMS: &[f32] = &[
    0.05, 0.0833, 0.125, 0.1667, 0.25, 0.3333, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0, 32.0,
];
/// 滚轮一格的缩放倍率（规格 F-100）。
const WHEEL_STEP: f32 = 1.18;
/// 收到 Wintab 数据包之后的这段时间里，认为笔是由 Wintab 管着的。
const WINTAB_HOLD: Duration = Duration::from_millis(300);
/// 色板最多存多少个颜色。
pub const MAX_SWATCHES: usize = 64;
/// 出厂色板：黑白灰、几个基本色相、两个肤色。
const SWATCHES: &[[u8; 3]] = &[
    [0, 0, 0],
    [64, 64, 64],
    [128, 128, 128],
    [192, 192, 192],
    [255, 255, 255],
    [214, 48, 49],
    [232, 126, 36],
    [241, 196, 15],
    [46, 160, 67],
    [23, 162, 184],
    [47, 111, 222],
    [125, 60, 190],
    [232, 100, 150],
    [121, 85, 61],
    [245, 209, 180],
    [160, 104, 72],
];
/// 缝隙闭合滑块的上限，像素。
pub const MAX_GAP: u32 = 10;
/// 按完"修饰键 + 按键"之后，这段时间内不理会修饰键（规格 F-151）。
const MODIFIER_GRACE: Duration = Duration::from_millis(300);

// 系统询问"这个窗口要不要笔的长按、轻拂等手势"的消息，以及回答"都不要"的标志位。
// 这些手势会让系统推迟报告落笔，对画布没有用处。
const WM_TABLET_QUERYSYSTEMGESTURESTATUS: u32 = 0x02CC;
const TABLET_DISABLE_ALL: isize = 0x0000_0001 | 0x0000_0008 | 0x0000_0010 | 0x0001_0000;
/// 由笔或触摸转换来的鼠标消息带有这个签名。
const MI_WP_SIGNATURE: isize = 0xFF51_5700;
const MI_WP_SIGNATURE_MASK: isize = 0xFFFF_FF00u32 as isize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Pen,
    Marker,
    Airbrush,
    Eraser,
    Bucket,
    Select,
    Lasso,
    Wand,
    Move,
}

impl Tool {
    pub const ALL: [Tool; 9] = [
        Tool::Pen,
        Tool::Marker,
        Tool::Airbrush,
        Tool::Eraser,
        Tool::Bucket,
        Tool::Select,
        Tool::Lasso,
        Tool::Wand,
        Tool::Move,
    ];
    /// 用笔刷引擎的四种工具。`App::brushes` 按这个顺序存它们的参数。
    const BRUSHES: [Tool; 4] = [Tool::Pen, Tool::Marker, Tool::Airbrush, Tool::Eraser];

    pub fn is_brush(self) -> bool {
        (self as usize) < Self::BRUSHES.len()
    }

    pub fn label(self) -> &'static str {
        match self {
            // 起草和勾线用同一支笔就够了，不分铅笔和钢笔（用户 2026-10-14）。
            Tool::Pen => tr("笔", "Pen"),
            Tool::Marker => tr("马克笔", "Marker"),
            Tool::Airbrush => tr("喷枪", "Airbrush"),
            Tool::Eraser => tr("橡皮", "Eraser"),
            Tool::Bucket => tr("油漆桶", "Bucket"),
            Tool::Select => tr("选框", "Select"),
            Tool::Lasso => tr("套索", "Lasso"),
            Tool::Wand => tr("魔棒", "Wand"),
            Tool::Move => tr("移动", "Move"),
        }
    }

    /// 笔刷类工具在设置文件里的名字。
    fn key(self) -> &'static str {
        match self {
            // 笔原来叫铅笔，设置文件里沿用旧名字，免得用户调好的参数丢了。
            Tool::Pen => "pencil",
            Tool::Marker => "marker",
            Tool::Airbrush => "airbrush",
            _ => "eraser",
        }
    }

    /// 出厂参数。四种笔刷工具是同一个笔刷引擎的四组预设（规格 F-010）。
    fn defaults(self) -> BrushParams {
        match self {
            Tool::Pen => BrushParams { size: 10.0, ..Default::default() },
            // 马克笔：粗细不随压力变，压力只管浓淡；硬边，上色时边缘干净（规格 F-031）。
            Tool::Marker => BrushParams { size: 30.0, min_size: 1.0, density: 1.0, min_density: 0.3, hardness: 1.0, marker: true, ..Default::default() },
            Tool::Airbrush => BrushParams { size: 60.0, min_size: 0.5, density: 0.5, min_density: 0.0, hardness: 0.0, ..Default::default() },
            _ => BrushParams { size: 30.0, min_size: 0.5, erase: true, ..Default::default() },
        }
    }
}

/// 界面和菜单能发出的命令。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cmd {
    New,
    Open,
    Save,
    SaveAs,
    Export,
    Exit,
    Undo,
    Redo,
    ClearLayer,
    LayerNew,
    LayerDup,
    LayerDel,
    LayerUp,
    LayerDown,
    LayerMerge,
    ZoomIn,
    ZoomOut,
    Zoom100,
    Fit,
    DefaultView,
    AngleZero,
    RotateCcw,
    RotateCw,
    Flip,
    TogglePanels,
    SwapColors,
    SelectTool(Tool),
    SizeDown,
    SizeUp,
    StabilizerDown,
    StabilizerUp,
    /// 打开快捷键对话框
    Shortcuts,
    ShowRgb,
    ShowHsv,
    ShowSwatches,
    PanelsRight,
    SwatchAdd,
    SwatchRemove,
    MergeVisible,
    Flatten,
    /// 用前景色填满当前图层（有选区时填选区）
    FillLayer,
    WhiteToAlpha,
    ToggleLockAlpha,
    ToggleLocked,
    ToggleClip,
    /// 笔输入走 Windows 的笔消息
    PenPointer,
    /// 笔输入走数位板驱动的 Wintab
    PenWintab,
    /// 轻于这个百分比的压力不出墨
    MinPressure(u8),
    /// 界面语言：true 为英文
    Language(bool),
    SelectAll,
    Deselect,
    InvertSelection,
    Grow,
    Shrink,
    ToggleOutline,
    /// 油漆桶和魔棒看哪里的颜色：true 是所有图层合起来的画面，false 是当前图层
    FillSource(bool),
    /// 进入自由变换
    Transform,
    XformOk,
    XformCancel,
    XformReset,
    FlipH,
    FlipV,
    TurnCcw,
    TurnCw,
}

impl Cmd {
    /// 变换进行中也照常执行、不会把变换了结掉的命令：看图的，和变换自己的。
    fn keeps_xform(self) -> bool {
        use Cmd::*;
        matches!(
            self,
            ZoomIn | ZoomOut | Zoom100 | Fit | DefaultView | AngleZero | RotateCcw | RotateCw | Flip | TogglePanels | SwapColors
                | SizeDown | SizeUp | StabilizerDown | StabilizerUp | ToggleOutline | ShowRgb | ShowHsv | ShowSwatches | PanelsRight
                | SwatchAdd | SwatchRemove | PenPointer | PenWintab | MinPressure(_) | Language(_) | Transform | XformOk | XformCancel | XformReset | FlipH | FlipV | TurnCcw | TurnCw
        )
    }
}

/// 菜单项：编号、中文、英文、命令。编号为 0 的是分隔线。快捷键提示不写在这里，建菜单时按当前键位补上。
type MenuItem = (u16, &'static str, &'static str, Cmd);

const MENUS: &[(&str, &str, &[MenuItem])] = &[
    (
        "文件",
        "File",
        &[
            (101, "新建…", "New…", Cmd::New),
            (102, "打开…", "Open…", Cmd::Open),
            (0, "", "", Cmd::Exit),
            (103, "保存", "Save", Cmd::Save),
            (104, "另存为…", "Save As…", Cmd::SaveAs),
            (105, "导出…", "Export…", Cmd::Export),
            (0, "", "", Cmd::Exit),
            (106, "退出", "Exit", Cmd::Exit),
        ],
    ),
    (
        "编辑",
        "Edit",
        &[
            (201, "撤销", "Undo", Cmd::Undo),
            (202, "重做", "Redo", Cmd::Redo),
            (0, "", "", Cmd::Exit),
            (203, "清除", "Clear", Cmd::ClearLayer),
            (206, "填充前景色", "Fill with Foreground Color", Cmd::FillLayer),
            (204, "自由变换", "Free Transform", Cmd::Transform),
            (0, "", "", Cmd::Exit),
            (205, "快捷键…", "Keyboard Shortcuts…", Cmd::Shortcuts),
        ],
    ),
    (
        "图层",
        "Layer",
        &[
            (301, "新建图层", "New Layer", Cmd::LayerNew),
            (302, "复制图层", "Duplicate Layer", Cmd::LayerDup),
            (303, "删除图层", "Delete Layer", Cmd::LayerDel),
            (0, "", "", Cmd::Exit),
            (304, "上移", "Move Up", Cmd::LayerUp),
            (305, "下移", "Move Down", Cmd::LayerDown),
            (306, "向下合并", "Merge Down", Cmd::LayerMerge),
            (311, "合并可见图层", "Merge Visible", Cmd::MergeVisible),
            (312, "拼合成一层", "Flatten", Cmd::Flatten),
            (0, "", "", Cmd::Exit),
            (307, "左右翻转", "Flip Horizontal", Cmd::FlipH),
            (308, "上下翻转", "Flip Vertical", Cmd::FlipV),
            (309, "左转 90°", "Rotate 90° Left", Cmd::TurnCcw),
            (310, "右转 90°", "Rotate 90° Right", Cmd::TurnCw),
            (0, "", "", Cmd::Exit),
            (313, "白底变透明", "White to Transparent", Cmd::WhiteToAlpha),
        ],
    ),
    (
        "选择",
        "Select",
        &[
            (501, "全选", "Select All", Cmd::SelectAll),
            (502, "取消选择", "Deselect", Cmd::Deselect),
            (503, "反选", "Invert Selection", Cmd::InvertSelection),
            (0, "", "", Cmd::Exit),
            (504, "扩大 1 像素", "Grow by 1 px", Cmd::Grow),
            (505, "收缩 1 像素", "Shrink by 1 px", Cmd::Shrink),
            (0, "", "", Cmd::Exit),
            (506, "显示或隐藏选区边线", "Show or Hide Selection Outline", Cmd::ToggleOutline),
        ],
    ),
    (
        "笔",
        "Pen",
        &[
            (601, "用 Windows 的笔输入", "Windows Pen Input", Cmd::PenPointer),
            (602, "用数位板驱动的 Wintab", "Wintab (Tablet Driver)", Cmd::PenWintab),
            (0, "", "", Cmd::Exit),
            (603, "轻压也出墨", "Light Pressure Draws", Cmd::MinPressure(0)),
            (604, "压力不到 2% 不出墨", "Ignore Pressure Below 2%", Cmd::MinPressure(2)),
            (605, "压力不到 5% 不出墨", "Ignore Pressure Below 5%", Cmd::MinPressure(5)),
            (606, "压力不到 10% 不出墨", "Ignore Pressure Below 10%", Cmd::MinPressure(10)),
        ],
    ),
    (
        "视图",
        "View",
        &[
            (401, "放大", "Zoom In", Cmd::ZoomIn),
            (402, "缩小", "Zoom Out", Cmd::ZoomOut),
            (403, "实际大小", "Actual Size", Cmd::Zoom100),
            (404, "适合窗口", "Fit to Window", Cmd::Fit),
            (405, "回到全图", "Reset View", Cmd::DefaultView),
            (0, "", "", Cmd::Exit),
            (406, "角度归零", "Reset Rotation", Cmd::AngleZero),
            (407, "镜像查看", "Mirror View", Cmd::Flip),
            (0, "", "", Cmd::Exit),
            (408, "隐藏或显示面板", "Show or Hide Panels", Cmd::TogglePanels),
            (412, "面板放在右边", "Panels on the Right", Cmd::PanelsRight),
            (0, "", "", Cmd::Exit),
            (409, "红绿蓝滑块", "RGB Sliders", Cmd::ShowRgb),
            (410, "色相、饱和度、明度滑块", "HSV Sliders", Cmd::ShowHsv),
            (411, "色板", "Swatches", Cmd::ShowSwatches),
            (0, "", "", Cmd::Exit),
            // 这两项不翻译：不管现在是哪种语言，都要认得出自己要的那一项。
            (413, "中文", "中文", Cmd::Language(false)),
            (414, "English", "English", Cmd::Language(true)),
        ],
    ),
];

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Pan,
    Rotate,
    Pick,
    /// 正在拖矩形选框
    Marquee,
    /// 正在圈套索
    Lasso,
    /// 正在拖变换的手柄（移动工具也走这里）
    Handle(Handle),
    /// 点一下就完成的操作（油漆桶、魔棒）已经做完，等松手
    Done,
}

/// 界面上正在被按住拖动的部件。
#[derive(Clone)]
enum Grab {
    Button(Cmd, RECT),
    Slider(SliderId, RECT),
    /// 色环的外圈（true）或中间方块（false）
    Wheel(bool, RECT),
}

/// 需要在放开应用状态之后才能做的事（它们有自己的消息循环）。
enum Later {
    Run(Cmd),
    Popup(Popup, POINT),
    Rename(usize, RECT),
}

struct Backbuffer {
    dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u32,
    size: (i32, i32),
}

pub struct App {
    hwnd: HWND,
    pub scale: f32,
    pub font: HFONT,
    pub doc: Document,
    path: Option<PathBuf>,
    /// `path` 是导入的 PSD 或 ORA。这时"保存"要另起一个我们自己格式的文件名，不去覆盖原文件。
    foreign: bool,
    display: Display,
    pub view: View,
    back: Option<Backbuffer>,
    layout: Layout,
    pub panels: bool,
    pub tool: Tool,
    brushes: [BrushParams; 4],
    /// 当前颜色的色相（0..360）、饱和度、明度。单独存着，这样把明度拖到 0 再拖回来色相不会丢。
    pub hsv: (f32, f32, f32),
    pub background: [u8; 3],
    /// 出厂不显示的几块颜色面板，和面板放哪一边
    pub show_rgb: bool,
    pub show_hsv: bool,
    pub show_swatches: bool,
    pub panels_right: bool,
    pub swatches: Vec<[u8; 3]>,
    pub stabilizer: usize,
    stroke: Option<Stroke>,
    drag: Drag,
    /// 选框的起点（画布坐标）
    drag_from: (f32, f32),
    /// 套索已经圈过的点（画布坐标）
    lasso: Vec<(f32, f32)>,
    /// 这次拖出来的选区怎么并进已有的选区
    combine: Combine,
    /// 进行中的移动或自由变换
    xform: Option<Xform>,
    /// 油漆桶和魔棒的设置
    pub tolerance: u8,
    pub gap: u32,
    pub fill_merged: bool,
    grab: Option<Grab>,
    /// 拖动图层不透明度期间，拖动开始前的属性
    props_before: Option<(usize, LayerProps)>,
    /// 上一个指针位置，画布视口内的坐标
    last_pos: (f32, f32),
    /// 光标圆环上次画在哪里（视口内坐标），下次要先擦掉
    ring: Rect,
    hovering: bool,
    /// 笔在感应范围内。这期间系统还会不时补发一些普通的鼠标移动消息（用来更新光标），
    /// 它们不带"来自笔"的签名；要是当成鼠标处理，会把正在画的这一笔打断。
    pen_near: bool,
    /// 打开着的 Wintab 数位板；None 表示笔输入走 Windows 的笔消息
    tablet: Option<wintab::Tablet>,
    /// 上一个 Wintab 数据包是什么时候来的
    wintab_last: Option<Instant>,
    /// 轻于这个值的压力不出墨（规格 F-162）
    min_pressure: f32,
    /// 上一次带修饰键的快捷键是什么时候按的
    last_combo: Option<Instant>,
    keymap: Keymap,
    /// 正按着的工具键：键码、按下之前的工具、按着的时候有没有用过。
    /// 按着画了再松开，就回到之前的工具（规格 F-152）；只按一下不画，就是换工具。
    held: Option<(u16, Tool, bool)>,
    /// 笔正在当橡皮用：换成橡皮之前的工具
    pen_eraser: Option<Tool>,
    /// 正在原地改名的图层和它的输入框
    rename: Option<(usize, HWND)>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

fn to_rect(r: &RECT) -> Rect {
    Rect { x0: r.left, y0: r.top, x1: r.right, y1: r.bottom }
}

impl App {
    /// 当前笔刷的参数。当前工具不是笔刷时给的是笔的，调用的地方不用再分情况。
    pub fn brush(&self) -> &BrushParams {
        &self.brushes[if self.tool.is_brush() { self.tool as usize } else { 0 }]
    }

    fn brush_mut(&mut self) -> &mut BrushParams {
        &mut self.brushes[if self.tool.is_brush() { self.tool as usize } else { 0 }]
    }

    /// 画布上要不要画笔刷大小的圆环：只有笔刷类工具才画。
    fn shows_ring(&self) -> bool {
        self.tool.is_brush() && self.xform.is_none()
    }

    fn fill_options(&self) -> FillOptions {
        let source = if self.fill_merged { Source::Merged } else { Source::Layer(self.doc.active()) };
        FillOptions { tolerance: self.tolerance, gap: self.gap, source }
    }

    /// 变换进行中时两个方向的缩放和角度（度），给面板显示用。
    pub fn xform_readout(&self) -> Option<(f32, f32, f32)> {
        self.xform.as_ref().filter(|x| !x.quick).map(|x| (x.scale.0.abs(), x.scale.1.abs(), x.angle.to_degrees()))
    }

    pub fn color(&self) -> [u8; 3] {
        ui::hsv_to_rgb(self.hsv.0, self.hsv.1, self.hsv.2)
    }

    fn client(&self) -> (i32, i32) {
        self.back.as_ref().map_or((1, 1), |b| b.size)
    }

    /// 画布视口的大小。
    fn vp(&self) -> (i32, i32) {
        let c = &self.layout.canvas;
        ((c.right - c.left).max(1), (c.bottom - c.top).max(1))
    }

    fn relayout(&mut self) {
        self.layout = ui::build(self, self.client());
    }

    fn update_title(&self) {
        let name = self.path.as_ref().and_then(|p| p.file_name()).map_or(tr("未命名", "Untitled").to_string(), |n| n.to_string_lossy().into_owned());
        let title = format!("{}{} — {}", if self.doc.is_modified() { "* " } else { "" }, name, tr("纸笔", "Pen & Paper"));
        let wide = dialogs::wide(&title);
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(wide.as_ptr()));
        }
    }

    fn resize(&mut self, w: i32, h: i32) {
        let (w, h) = (w.max(1), h.max(1));
        if self.back.as_ref().is_some_and(|b| b.size == (w, h)) {
            return;
        }
        unsafe {
            if let Some(b) = self.back.take() {
                let _ = DeleteDC(b.dc);
                let _ = DeleteObject(b.bitmap.into());
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // 自上而下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let dc = CreateCompatibleDC(None);
            let mut bits = std::ptr::null_mut();
            if let Ok(bitmap) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                SelectObject(dc, bitmap.into());
                self.back = Some(Backbuffer { dc, bitmap, bits: bits as *mut u32, size: (w, h) });
            }
        }
        self.ring = Rect::EMPTY;
        self.relayout();
    }

    fn ring_radius(&self) -> f32 {
        // 画不上的时候圆环里要画一道斜杠，太小了看不出来。
        let least = if self.doc.can_edit_active() { 1.5 } else { 8.0 };
        (self.brush().size * self.view.zoom / 2.0).max(least)
    }

    /// 把画布视口里的一块（视口内坐标）重画进后台缓冲，叠上光标圆环，再贴到窗口上。
    fn present(&mut self, rect: Rect, hdc: Option<HDC>) {
        let vp = self.vp();
        let rect = rect.intersect(Rect { x0: 0, y0: 0, x1: vp.0, y1: vp.1 });
        let Some(back) = self.back.as_ref() else { return };
        if rect.is_empty() {
            return;
        }
        let origin = (self.layout.canvas.left, self.layout.canvas.top);
        let stride = back.size.0 as usize;
        let buf = unsafe { std::slice::from_raw_parts_mut(back.bits, (back.size.0 * back.size.1) as usize) };
        self.display.render(&self.doc, self.stroke.as_ref(), &self.view, vp, buf, stride, origin, rect);
        // 正在拖的选框、套索和变换的外框，叠在画面上。
        let to_screen = |c: (f32, f32)| self.view.to_screen(c, vp);
        let mut outline = |q: &[(f32, f32)]| {
            for i in 0..q.len() {
                dashed(buf, stride, origin, rect, q[i], q[(i + 1) % q.len()]);
            }
        };
        match self.drag {
            Drag::Marquee => {
                let r = marquee(self.drag_from, self.view.to_canvas(self.last_pos, vp));
                let (x0, y0, x1, y1) = (r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32);
                outline(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(to_screen));
            }
            Drag::Lasso => outline(&self.lasso.iter().map(|p| to_screen(*p)).collect::<Vec<_>>()),
            _ => {}
        }
        if let Some(x) = self.xform.as_ref().filter(|x| !x.quick) {
            outline(&x.corners().map(to_screen));
            let half = (4.0 * self.scale) as i32;
            for (hx, hy) in [(-1, -1), (0, -1), (1, -1), (1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0)] {
                knob(buf, stride, origin, rect, to_screen(x.handle(hx, hy)), half);
            }
        }
        if self.hovering && self.drag == Drag::None && self.shows_ring() {
            let r = self.ring_radius();
            draw_ring(buf, stride, origin, vp, self.last_pos, r);
            // 当前图层锁定或者隐藏着，画不上：圆环里加一道斜杠。
            if !self.doc.can_edit_active() {
                let (c, d) = (self.last_pos, r * 0.7);
                dashed(buf, stride, origin, Rect { x0: 0, y0: 0, x1: vp.0, y1: vp.1 }, (c.0 - d, c.1 + d), (c.0 + d, c.1 - d));
            }
            self.ring = Rect {
                x0: (self.last_pos.0 - r).floor() as i32 - 2,
                y0: (self.last_pos.1 - r).floor() as i32 - 2,
                x1: (self.last_pos.0 + r).ceil() as i32 + 3,
                y1: (self.last_pos.1 + r).ceil() as i32 + 3,
            };
        } else {
            self.ring = Rect::EMPTY;
        }
        unsafe {
            let dc = hdc.unwrap_or_else(|| GetDC(Some(self.hwnd)));
            let (x, y) = (origin.0 + rect.x0, origin.1 + rect.y0);
            let _ = BitBlt(dc, x, y, rect.x1 - rect.x0, rect.y1 - rect.y0, Some(back.dc), x, y, SRCCOPY);
            if hdc.is_none() {
                ReleaseDC(Some(self.hwnd), dc);
            }
        }
    }

    /// 整个窗口重画：界面和画布。
    fn paint(&mut self, hdc: HDC, dirty: &RECT) {
        self.flush_xform();
        let Some(back) = self.back.as_ref() else { return };
        let (dc, size) = (back.dc, back.size);
        let buf = unsafe { std::slice::from_raw_parts_mut(back.bits, (size.0 * size.1) as usize) };
        ui::paint(self, &self.layout, dc, buf, size.0 as usize);
        unsafe {
            let _ = BitBlt(hdc, 0, 0, size.0, self.layout.top.bottom, Some(dc), 0, 0, SRCCOPY);
            // 面板在画布的左边或右边：两边都贴，没有面板的那一边宽度是零。
            let (top, c) = (self.layout.top.bottom, self.layout.canvas);
            let _ = BitBlt(hdc, 0, top, c.left, size.1, Some(dc), 0, top, SRCCOPY);
            let _ = BitBlt(hdc, c.right, top, (size.0 - c.right).max(0), size.1, Some(dc), c.right, top, SRCCOPY);
        }
        let c = to_rect(&self.layout.canvas).intersect(to_rect(dirty));
        if !c.is_empty() {
            let (ox, oy) = (self.layout.canvas.left, self.layout.canvas.top);
            self.present(Rect { x0: c.x0 - ox, y0: c.y0 - oy, x1: c.x1 - ox, y1: c.y1 - oy }, Some(hdc));
        }
    }

    /// 界面上的数值变了：重排、重画界面部分。
    fn refresh_ui(&mut self) {
        self.relayout();
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), Some(&self.layout.top), false);
            if self.panels {
                let _ = InvalidateRect(Some(self.hwnd), Some(&self.layout.left), false);
                let _ = InvalidateRect(Some(self.hwnd), Some(&self.layout.layers), false);
            }
        }
        self.update_title();
    }

    /// 视图或整张画布变了：全部重画。
    fn refresh_all(&mut self) {
        self.relayout();
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
        self.update_title();
    }

    /// 画布上的一块变了：让对应的显示缓存失效，并立刻重画屏幕上的那一块（连同光标圆环）。
    fn canvas_changed(&mut self, canvas: Rect) {
        self.display.invalidate(canvas);
        let screen = self.view.screen_bounds(canvas, self.vp());
        let old_ring = self.ring;
        let r = self.ring_radius() + 3.0;
        let new_ring = if self.shows_ring() {
            Rect {
                x0: (self.last_pos.0 - r).floor() as i32,
                y0: (self.last_pos.1 - r).floor() as i32,
                x1: (self.last_pos.0 + r).ceil() as i32,
                y1: (self.last_pos.1 + r).ceil() as i32,
            }
        } else {
            Rect::EMPTY
        };
        // 两个圆环相距很远时分开重画，免得中间一大片白白重画。
        if screen.is_empty() && !old_ring.is_empty() {
            self.present(old_ring, None);
            self.present(new_ring, None);
        } else {
            self.present(screen.union(old_ring).union(new_ring), None);
        }
    }

    /// 整张画布的内容都可能变了（图层结构、属性）。
    fn document_changed(&mut self) {
        self.display.invalidate_all();
        self.refresh_all();
    }

    /// 修饰键当前是否算数。刚按完"修饰键 + 按键"时手指往往还没松开，
    /// 这时紧接着的一笔不应该被当成别的操作（规格 F-151）。
    fn modifiers_live(&self) -> bool {
        self.last_combo.is_none_or(|t| t.elapsed() > MODIFIER_GRACE)
    }

    fn pick_color(&mut self, pos: (f32, f32)) {
        let c = self.view.to_canvas(pos, self.vp());
        if let Some(rgb) = self.doc.pick(c.0.floor() as i32, c.1.floor() as i32) {
            self.hsv = ui::rgb_to_hsv(rgb);
            self.refresh_ui();
        }
    }

    /// 笔现在是不是由 Wintab 管着。是的话，同一支笔经 Windows 送来的消息就不再理会，免得一笔画两遍。
    fn wintab_live(&self) -> bool {
        self.wintab_last.is_some_and(|t| t.elapsed() < WINTAB_HOLD)
    }

    /// 轻于最小压力的不算，其余的重新铺满 0 到 1。
    fn pressure(&self, raw: f32) -> f32 {
        let min = self.min_pressure;
        if raw <= min {
            0.0
        } else {
            (raw - min) / (1.0 - min)
        }
    }

    /// 一批笔样本，按时间先后。`origin` 是窗口客户区左上角在屏幕上的位置；`lifted` 表示这批之后笔尖抬起了。
    /// 返回 false 表示笔不在画布上，这批样本不归画布管。
    fn pen(&mut self, origin: POINT, samples: &[wintab::PenSample], lifted: bool) -> bool {
        self.pen_near = true;
        let c = self.layout.canvas;
        let Some(&(newest, _, _, eraser)) = samples.last() else { return false };
        let client = (newest.0 - origin.x as f32, newest.1 - origin.y as f32);
        let busy = self.stroke.is_some() || self.drag != Drag::None;
        if !busy && (self.grab.is_some() || !ui::contains(&c, client.0 as i32, client.1 as i32)) {
            if self.hovering {
                self.hovering = false;
                let old = self.ring;
                self.present(old, None);
            }
            return false;
        }
        // 笔当橡皮用的时候临时换成橡皮工具，回到笔尖时换回原来的。只在两笔之间换，不打断正在画的一笔。
        if self.stroke.is_none() && self.drag == Drag::None && self.xform.is_none() && eraser != self.pen_eraser.is_some() {
            if eraser {
                self.pen_eraser = Some(self.tool);
                self.exec(Cmd::SelectTool(Tool::Eraser));
            } else if let Some(before) = self.pen_eraser.take() {
                // 当橡皮期间自己又换过工具的话，就不再换回去了。
                if self.tool == Tool::Eraser {
                    self.exec(Cmd::SelectTool(before));
                }
            }
        }
        let mut dirty = Rect::EMPTY;
        for (at, pressure, contact, _) in samples {
            let pos = (at.0 - origin.x as f32 - c.left as f32, at.1 - origin.y as f32 - c.top as f32);
            dirty = dirty.union(self.input(pos, self.pressure(*pressure), *contact && !lifted));
        }
        if lifted {
            dirty = dirty.union(self.input(self.last_pos, 0.0, false));
        }
        if self.drag == Drag::None {
            self.canvas_changed(dirty);
        }
        true
    }

    /// 画布上的一个输入样本（视口内坐标，压力 0 到 1）。`contact` 表示笔尖或鼠标键是否按下。
    fn input(&mut self, pos: (f32, f32), pressure: f32, contact: bool) -> Rect {
        let prev = self.last_pos;
        self.last_pos = pos;
        self.hovering = true;
        let vp = self.vp();
        let c = self.view.to_canvas(pos, vp);

        if self.drag != Drag::None {
            if !contact {
                self.end_drag(c);
                return Rect::EMPTY;
            }
            match self.drag {
                Drag::Pan => self.view.pan((pos.0 - prev.0, pos.1 - prev.1)),
                Drag::Rotate => {
                    let mid = (vp.0 as f32 / 2.0, vp.1 as f32 / 2.0);
                    let a0 = (prev.1 - mid.1).atan2(prev.0 - mid.0);
                    let a1 = (pos.1 - mid.1).atan2(pos.0 - mid.0);
                    self.view.angle = wrap_angle(self.view.angle + a1 - a0);
                }
                Drag::Pick => {
                    self.pick_color(pos);
                    return Rect::EMPTY;
                }
                Drag::Lasso => self.lasso.push(c),
                Drag::Handle(handle) => {
                    let shift = key_down(VK_SHIFT.0);
                    if let Some(x) = self.xform.as_mut() {
                        x.drag(handle, c, shift);
                    }
                }
                Drag::Marquee => {}
                Drag::Done | Drag::None => return Rect::EMPTY,
            }
            self.refresh_all();
            return Rect::EMPTY;
        }

        let sample = Sample { x: c.0, y: c.1, p: pressure };
        match (&mut self.stroke, contact) {
            (None, true) => self.press(pos, c, sample),
            (Some(stroke), true) => stroke.add(sample),
            (Some(_), false) => self.finish_stroke(),
            (None, false) => Rect::EMPTY,
        }
    }

    /// 落笔（或按下鼠标）的那一下：看修饰键和当前工具，决定这是什么操作。
    fn press(&mut self, pos: (f32, f32), c: (f32, f32), sample: Sample) -> Rect {
        if let Some(held) = self.held.as_mut() {
            held.2 = true;
        }
        let live = self.modifiers_live();
        let (space, shift) = (key_down(VK_SPACE.0), key_down(VK_SHIFT.0));
        let (alt, ctrl) = (key_down(VK_MENU.0) && live, key_down(VK_CONTROL.0) && live);
        let (vp, view) = (self.vp(), self.view);
        // 按住空格是平移，再加 Alt 是旋转；松手就回到原来的工具（规格 F-101、F-102）。
        if space {
            self.drag = if alt { Drag::Rotate } else { Drag::Pan };
            self.refresh_all();
            return Rect::EMPTY;
        }
        // 变换进行中，画布上只认外框和手柄。
        let reach = 9.0 * self.scale;
        if let Some(x) = self.xform.as_mut() {
            let handle = x.hit(pos, |p| view.to_screen(p, vp), reach);
            x.grab(c);
            self.drag = Drag::Handle(handle);
            return Rect::EMPTY;
        }
        // 移动工具；用别的工具时按住 Ctrl 拖动也是移动（规格 F-081）。
        if ctrl || self.tool == Tool::Move {
            self.drag = Drag::Done;
            if self.begin_xform(true) {
                if let Some(x) = self.xform.as_mut() {
                    x.grab(c);
                }
                self.drag = Drag::Handle(Handle::Move);
            }
            return Rect::EMPTY;
        }
        let how = if shift {
            Combine::Add
        } else if alt {
            Combine::Subtract
        } else {
            Combine::Replace
        };
        let at = (c.0.floor() as i32, c.1.floor() as i32);
        match self.tool {
            Tool::Select => (self.drag, self.drag_from, self.combine) = (Drag::Marquee, c, how),
            Tool::Lasso => {
                (self.drag, self.combine) = (Drag::Lasso, how);
                self.lasso.clear();
                self.lasso.push(c);
            }
            Tool::Wand => {
                self.drag = Drag::Done;
                let found = fill::flood_select(&self.doc, at.0, at.1, &self.fill_options());
                self.select(found, how);
                self.refresh_all();
            }
            // 画的工具单按 Alt 是吸管（规格 F-092）。选的工具上 Alt 是减选，上面已经用掉了。
            _ if alt => {
                self.drag = Drag::Pick;
                self.pick_color(pos);
                self.refresh_all();
            }
            // 当前图层锁定或者隐藏着：不开始这一笔。
            _ if !self.doc.can_edit_active() => self.drag = Drag::Done,
            Tool::Bucket => {
                self.drag = Drag::Done;
                let (color, opts) = (self.color(), self.fill_options());
                let dirty = fill::bucket_fill(&mut self.doc, at.0, at.1, color, 1.0, &opts);
                if !dirty.is_empty() {
                    self.after_history(dirty);
                }
            }
            _ => {
                let mut params = *self.brush();
                params.color = self.color();
                let (stroke, dirty) = Stroke::begin(params, self.stabilizer, sample);
                self.stroke = Some(stroke);
                return dirty;
            }
        }
        Rect::EMPTY
    }

    /// 松手：把拖出来的东西落实。
    fn end_drag(&mut self, c: (f32, f32)) {
        let bounds = self.doc.bounds();
        let made = match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Marquee => {
                let r = marquee(self.drag_from, c).intersect(bounds);
                // 没拖开就松手，等于在空白处点了一下。
                Some(if (r.x1 - r.x0).max(r.y1 - r.y0) < 2 { Selection::empty() } else { Selection::rect(r) })
            }
            Drag::Lasso => {
                let made = Selection::polygon(&self.lasso, bounds);
                self.lasso.clear();
                Some(made)
            }
            Drag::Handle(_) => {
                if self.xform.as_ref().is_some_and(|x| x.quick) {
                    self.end_xform(true);
                }
                None
            }
            _ => None,
        };
        // 点一下空白处是取消选区；加选、减选时点空了就当没点。
        if let Some(made) = made.filter(|m| !m.is_empty() || self.combine == Combine::Replace) {
            self.select(made, self.combine);
        }
        self.refresh_all();
    }

    /// 把新圈出来的范围并进当前选区，或者从里面挖掉。
    fn select(&mut self, made: Selection, how: Combine) {
        let merged = match (self.doc.selection(), how) {
            (Some(cur), Combine::Add | Combine::Subtract) => cur.combined(&made, how),
            (None, Combine::Subtract) => Selection::empty(),
            _ => made,
        };
        self.doc.set_selection(Some(merged));
    }

    /// 开始移动或自由变换（规格 F-080、F-081）。当前图层（有选区时是选中的部分）没有内容时什么也不做。
    fn begin_xform(&mut self, quick: bool) -> bool {
        if self.xform.is_none() {
            if !self.doc.can_edit_active() {
                return false;
            }
            let Some(x) = Xform::begin(&self.doc, quick) else { return false };
            // 变换期间选区跟着内容走，原来位置上的边线不再显示。
            self.doc.set_selection(None);
            self.xform = Some(x);
        }
        true
    }

    /// 结束变换。`keep` 为 false 时放弃，像素和选区都回到开始前的样子。
    fn end_xform(&mut self, keep: bool) {
        let Some(x) = self.xform.take() else { return };
        let clip = self.doc.bounds();
        // 预览期间图层里放的是预览用的像素，先换回原图。
        self.doc.set_layer_tiles_live(x.layer, x.orig.clone());
        self.doc.set_selection(x.sel.clone());
        if keep && !x.is_identity() {
            let tiles = x.render(Quality::Best, clip);
            match x.moved_selection(clip) {
                Some(sel) => self.doc.replace_layer_tiles_and_selection(x.layer, tiles, Some(sel)),
                None => self.doc.replace_layer_tiles(x.layer, tiles),
            };
        }
        self.document_changed();
    }

    /// 变换的参数变了之后，把预览像素重算一遍。放在重画之前做，这样连着来的几次拖动只算一次。
    fn flush_xform(&mut self) {
        let clip = self.doc.bounds();
        let Some(x) = self.xform.as_mut().filter(|x| x.stale) else { return };
        x.stale = false;
        let (layer, tiles) = (x.layer, x.render(Quality::Fast, clip));
        self.doc.set_layer_tiles_live(layer, tiles);
        self.display.invalidate_all();
    }

    fn finish_stroke(&mut self) -> Rect {
        let Some(mut stroke) = self.stroke.take() else { return Rect::EMPTY };
        let tail = stroke.end();
        let was_modified = self.doc.is_modified();
        let could_undo = self.doc.can_undo();
        let committed = self.doc.commit_stroke(&stroke);
        if was_modified != self.doc.is_modified() || could_undo != self.doc.can_undo() {
            self.refresh_ui();
        }
        tail.union(committed)
    }

    fn zoom_to(&mut self, zoom: f32, anchor: (f32, f32)) {
        let vp = self.vp();
        self.view.zoom_at(zoom, anchor, vp);
        self.refresh_all();
    }

    fn step_zoom(&mut self, up: bool, anchor: (f32, f32)) {
        let z = self.view.zoom;
        let next = if up { ZOOMS.iter().copied().find(|&s| s > z * 1.001) } else { ZOOMS.iter().rev().copied().find(|&s| s < z * 0.999) };
        if let Some(next) = next {
            self.zoom_to(next, anchor);
        }
    }

    fn step_size(&mut self, up: bool) {
        if !self.tool.is_brush() {
            return;
        }
        let cur = self.brush().size;
        let next = if up { SIZES.iter().copied().find(|&s| s > cur + 0.01) } else { SIZES.iter().rev().copied().find(|&s| s < cur - 0.01) };
        if let Some(next) = next {
            self.brush_mut().size = next;
            self.canvas_changed(Rect::EMPTY);
            self.refresh_ui();
        }
    }

    /// 执行一条命令。需要弹对话框的命令原样交回，由调用者在放开应用状态之后处理。
    fn exec(&mut self, cmd: Cmd) -> Option<Later> {
        // 变换进行中：看图的命令照常执行，别的命令先把变换确认下来。撤销在这时等于取消变换。
        if self.xform.is_some() && !cmd.keeps_xform() {
            self.drag = Drag::None;
            self.end_xform(cmd != Cmd::Undo);
            if cmd == Cmd::Undo {
                return None;
            }
        }
        let vp = self.vp();
        let centre = (vp.0 as f32 / 2.0, vp.1 as f32 / 2.0);
        let bounds = self.doc.bounds();
        match cmd {
            Cmd::New | Cmd::Open | Cmd::Save | Cmd::SaveAs | Cmd::Export | Cmd::Exit | Cmd::Shortcuts | Cmd::PenWintab => {
                return Some(Later::Run(cmd))
            }
            Cmd::PenPointer => {
                self.tablet = None;
                self.wintab_last = None;
                self.sync_menu();
            }
            Cmd::Language(english) => {
                lang::set_english(english);
                return Some(Later::Run(cmd));
            }
            Cmd::MinPressure(percent) => {
                self.min_pressure = percent as f32 / 100.0;
                self.sync_menu();
            }
            Cmd::Undo => {
                let dirty = self.doc.undo();
                self.after_history(dirty);
            }
            Cmd::Redo => {
                let dirty = self.doc.redo();
                self.after_history(dirty);
            }
            Cmd::ClearLayer => {
                let dirty = self.doc.clear_active();
                self.after_history(dirty);
            }
            Cmd::LayerNew => {
                self.doc.add_layer();
                self.document_changed();
            }
            Cmd::LayerDup => {
                self.doc.duplicate_active();
                self.document_changed();
            }
            Cmd::LayerDel => {
                self.doc.remove_active();
                self.document_changed();
            }
            Cmd::LayerUp => {
                self.doc.move_active(1);
                self.document_changed();
            }
            Cmd::LayerDown => {
                self.doc.move_active(-1);
                self.document_changed();
            }
            Cmd::LayerMerge => {
                self.doc.merge_down();
                self.document_changed();
            }
            Cmd::MergeVisible => {
                self.doc.merge_visible();
                self.document_changed();
            }
            Cmd::Flatten => {
                self.doc.flatten();
                self.document_changed();
            }
            Cmd::FillLayer => {
                let color = self.color();
                self.doc.fill_active(color);
                self.document_changed();
            }
            Cmd::WhiteToAlpha => {
                self.doc.white_to_alpha();
                self.document_changed();
            }
            Cmd::ToggleLockAlpha | Cmd::ToggleLocked | Cmd::ToggleClip => {
                let index = self.doc.active();
                let before = self.doc.layers()[index].props.clone();
                let mut props = before.clone();
                match cmd {
                    Cmd::ToggleLockAlpha => props.lock_alpha = !props.lock_alpha,
                    Cmd::ToggleLocked => props.locked = !props.locked,
                    _ => props.clip = !props.clip,
                }
                self.doc.set_props_live(index, props);
                self.doc.commit_props(index, before);
                self.document_changed();
            }
            Cmd::ZoomIn => self.step_zoom(true, if self.hovering { self.last_pos } else { centre }),
            Cmd::ZoomOut => self.step_zoom(false, if self.hovering { self.last_pos } else { centre }),
            Cmd::Zoom100 => self.zoom_to(1.0, centre),
            Cmd::Fit => {
                self.view.fit((self.doc.width, self.doc.height), vp);
                self.refresh_all();
            }
            Cmd::DefaultView => {
                // 默认视图：不旋转，整张画布居中（规格 F-105）。
                self.view.angle = 0.0;
                self.view.fit((self.doc.width, self.doc.height), vp);
                self.refresh_all();
            }
            Cmd::AngleZero => {
                self.view.angle = 0.0;
                self.refresh_all();
            }
            Cmd::RotateCcw | Cmd::RotateCw => {
                let step = 15f32.to_radians() * if cmd == Cmd::RotateCw { 1.0 } else { -1.0 };
                self.view.angle = wrap_angle(self.view.angle + step);
                self.refresh_all();
            }
            Cmd::Flip => {
                self.view.flip = !self.view.flip;
                self.refresh_all();
            }
            Cmd::TogglePanels => {
                self.panels = !self.panels;
                self.refresh_all();
            }
            Cmd::SwapColors => {
                let fg = self.color();
                self.hsv = ui::rgb_to_hsv(self.background);
                self.background = fg;
                self.refresh_ui();
            }
            Cmd::SelectTool(tool) => {
                self.tool = tool;
                self.refresh_all();
            }
            Cmd::ShowRgb | Cmd::ShowHsv | Cmd::ShowSwatches | Cmd::PanelsRight => {
                let flag = match cmd {
                    Cmd::ShowRgb => &mut self.show_rgb,
                    Cmd::ShowHsv => &mut self.show_hsv,
                    Cmd::ShowSwatches => &mut self.show_swatches,
                    _ => &mut self.panels_right,
                };
                *flag = !*flag;
                self.sync_menu();
                self.refresh_all();
            }
            Cmd::SwatchAdd | Cmd::SwatchRemove => {
                let color = self.color();
                self.swatches.retain(|c| *c != color);
                if cmd == Cmd::SwatchAdd && self.swatches.len() < MAX_SWATCHES {
                    self.swatches.push(color);
                }
                self.refresh_ui();
            }
            Cmd::SizeDown | Cmd::SizeUp => self.step_size(cmd == Cmd::SizeUp),
            Cmd::StabilizerDown | Cmd::StabilizerUp => {
                self.stabilizer = if cmd == Cmd::StabilizerUp { (self.stabilizer + 1).min(15) } else { self.stabilizer.saturating_sub(1) };
                self.refresh_ui();
            }
            Cmd::SelectAll => {
                self.doc.set_selection(Some(Selection::rect(bounds)));
                self.refresh_all();
            }
            Cmd::Deselect => {
                self.doc.set_selection(None);
                self.refresh_all();
            }
            Cmd::InvertSelection => {
                let inverted = self.doc.selection().map_or_else(|| Selection::rect(bounds), |s| s.inverted(bounds));
                self.doc.set_selection(Some(inverted));
                self.refresh_all();
            }
            Cmd::Grow | Cmd::Shrink => {
                let by = if cmd == Cmd::Grow { 1 } else { -1 };
                if let Some(changed) = self.doc.selection().map(|s| s.grown(by, bounds)) {
                    self.doc.set_selection(Some(changed));
                    self.refresh_all();
                }
            }
            Cmd::ToggleOutline => {
                self.display.hide_selection = !self.display.hide_selection;
                self.refresh_all();
            }
            Cmd::FillSource(merged) => {
                self.fill_merged = merged;
                self.refresh_ui();
            }
            Cmd::Transform => {
                if self.begin_xform(false) {
                    self.refresh_all();
                }
            }
            Cmd::XformOk | Cmd::XformCancel => {
                self.drag = Drag::None;
                self.end_xform(cmd == Cmd::XformOk);
            }
            Cmd::XformReset => {
                if let Some(x) = self.xform.as_mut() {
                    x.reset();
                }
                self.refresh_all();
            }
            Cmd::FlipH | Cmd::FlipV | Cmd::TurnCcw | Cmd::TurnCw => {
                // 不在变换里时，这几条是一步到位的图层操作（规格 F-082）；在变换里时只是改当前的变换。
                let single = self.xform.is_none();
                if self.begin_xform(false) {
                    if let Some(x) = self.xform.as_mut() {
                        match cmd {
                            Cmd::FlipH => x.flip(true),
                            Cmd::FlipV => x.flip(false),
                            _ => x.turn(cmd == Cmd::TurnCw),
                        }
                    }
                    if single {
                        self.end_xform(true);
                    } else {
                        self.refresh_all();
                    }
                }
            }
        }
        None
    }

    /// 撤销、重做、清空之后：图层结构可能也变了，界面要跟着重排。
    fn after_history(&mut self, dirty: Rect) {
        if dirty == self.doc.bounds() || dirty.is_empty() {
            self.document_changed();
        } else {
            self.canvas_changed(dirty);
            self.refresh_ui();
        }
    }

    fn set_slider(&mut self, id: SliderId, rect: &RECT, x: i32) {
        let t = ((x - rect.left) as f32 / (rect.right - rect.left).max(1) as f32).clamp(0.0, 1.0);
        let pct = (t * 100.0).round() / 100.0;
        match id {
            SliderId::Size => self.brush_mut().size = ui::slider_to_size(t),
            SliderId::MinSize => self.brush_mut().min_size = pct,
            SliderId::Density => self.brush_mut().density = pct,
            SliderId::MinDensity => self.brush_mut().min_density = pct,
            SliderId::Hardness => self.brush_mut().hardness = pct,
            SliderId::Rgb(i) => {
                let mut c = self.color();
                c[i as usize] = (t * 255.0).round() as u8;
                // 拖成灰色时色相没有意义，留着原来的，免得色环上的标记乱跳。
                let (h, s, v) = ui::rgb_to_hsv(c);
                self.hsv = (if s == 0.0 { self.hsv.0 } else { h }, s, v);
            }
            SliderId::Hsv(0) => self.hsv.0 = (t * 360.0).min(359.9),
            SliderId::Hsv(1) => self.hsv.1 = t,
            SliderId::Hsv(_) => self.hsv.2 = t,
            SliderId::Tolerance => self.tolerance = (t * 255.0).round() as u8,
            SliderId::Gap => self.gap = (t * MAX_GAP as f32).round() as u32,
            SliderId::LayerOpacity => {
                let index = self.doc.active();
                let props = self.doc.layers()[index].props.clone();
                if self.props_before.is_none() {
                    self.props_before = Some((index, props.clone()));
                }
                self.doc.set_props_live(index, LayerProps { opacity: pct, ..props });
                self.document_changed();
                return;
            }
        }
        self.refresh_ui();
    }

    fn set_wheel(&mut self, ring: bool, rect: &RECT, x: i32, y: i32) {
        let (c, _, _, half) = ui::wheel_geometry(rect);
        let (dx, dy) = (x as f32 + 0.5 - c.0, y as f32 + 0.5 - c.1);
        if ring {
            self.hsv.0 = dy.atan2(dx).to_degrees().rem_euclid(360.0);
        } else {
            self.hsv.1 = ((dx + half) / (2.0 * half)).clamp(0.0, 1.0);
            self.hsv.2 = (1.0 - (dy + half) / (2.0 * half)).clamp(0.0, 1.0);
        }
        self.refresh_ui();
    }

    /// 界面区域里的鼠标（或由笔转换来的鼠标）消息。
    fn ui_mouse(&mut self, msg: u32, x: i32, y: i32) -> Option<Later> {
        match msg {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                let hit = self.layout.items.iter().rev().find(|i| ui::contains(&i.rect, x, y))?;
                let rect = hit.rect;
                match &hit.kind {
                    Kind::Button { cmd, enabled, .. } => {
                        if *enabled {
                            self.grab = Some(Grab::Button(*cmd, rect));
                        }
                    }
                    Kind::Menu { which, .. } => return Some(Later::Popup(*which, POINT { x: rect.left, y: rect.bottom })),
                    Kind::Slider { id, .. } => {
                        let id = *id;
                        self.grab = Some(Grab::Slider(id, rect));
                        self.set_slider(id, &rect, x);
                    }
                    Kind::Wheel => {
                        let (c, _, inner, _) = ui::wheel_geometry(&rect);
                        let d = ((x as f32 - c.0).powi(2) + (y as f32 - c.1).powi(2)).sqrt();
                        let ring = d >= inner - 2.0;
                        self.grab = Some(Grab::Wheel(ring, rect));
                        self.set_wheel(ring, &rect, x, y);
                    }
                    Kind::Swatch { fg } => {
                        if !*fg {
                            return self.exec(Cmd::SwapColors);
                        }
                    }
                    Kind::Color { rgb, .. } => {
                        self.hsv = ui::rgb_to_hsv(*rgb);
                        self.refresh_ui();
                    }
                    Kind::SizeCell { size, .. } => {
                        self.brush_mut().size = *size;
                        self.refresh_ui();
                    }
                    Kind::LayerRow { index } => {
                        let index = *index;
                        if ui::contains(&ui::eye_rect(&rect, self.scale), x, y) {
                            let before = self.doc.layers()[index].props.clone();
                            self.doc.set_props_live(index, LayerProps { visible: !before.visible, ..before.clone() });
                            self.doc.commit_props(index, before);
                            self.document_changed();
                        } else {
                            // 换图层之前先把进行中的变换确认下来，免得它留在别的图层上。
                            if self.xform.is_some() && index != self.doc.active() {
                                self.end_xform(true);
                            }
                            self.doc.set_active(index);
                            self.refresh_ui();
                            if msg == WM_LBUTTONDBLCLK {
                                return Some(Later::Rename(index, ui::name_rect(&rect, self.scale)));
                            }
                        }
                    }
                    Kind::Label(_) => {}
                }
            }
            WM_MOUSEMOVE => match self.grab.clone() {
                Some(Grab::Slider(id, rect)) => self.set_slider(id, &rect, x),
                Some(Grab::Wheel(ring, rect)) => self.set_wheel(ring, &rect, x, y),
                _ => {}
            },
            WM_LBUTTONUP => {
                let grab = self.grab.take();
                if let Some((index, before)) = self.props_before.take() {
                    self.doc.commit_props(index, before);
                    self.refresh_ui();
                }
                if let Some(Grab::Button(cmd, rect)) = grab {
                    if ui::contains(&rect, x, y) {
                        return self.exec(cmd);
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// 键盘。返回 None 表示这个键我们不管。
    fn key(&mut self, vk: u16) -> Option<Option<Later>> {
        let (ctrl, shift, alt) = (key_down(VK_CONTROL.0), key_down(VK_SHIFT.0), key_down(VK_MENU.0));
        // 回车、Esc 和单按的数字键是固定的，其余的查键位表。
        let cmd = if vk == VK_RETURN.0 && self.xform.is_some() {
            Cmd::XformOk
        } else if vk == VK_ESCAPE.0 && self.xform.is_some() {
            Cmd::XformCancel
        } else if vk == VK_ESCAPE.0 && matches!(self.drag, Drag::Marquee | Drag::Lasso) {
            // 选框或套索拖到一半按 Esc：这一次不算。
            self.drag = Drag::Done;
            self.lasso.clear();
            self.refresh_all();
            return Some(None);
        } else if let Some(cmd) = self.keymap.find(Key { vk, ctrl, shift, alt }) {
            cmd
        } else if !ctrl && !alt && (b'0' as u16..=b'9' as u16).contains(&vk) && self.tool.is_brush() {
            // 数字键设浓度：1 到 9 是 10% 到 90%，0 是 100%（规格 F-029）。
            let d = (vk - b'0' as u16) as f32;
            self.brush_mut().density = if d == 0.0 { 1.0 } else { d / 10.0 };
            self.refresh_ui();
            return Some(None);
        } else {
            return None;
        };
        if ctrl || alt {
            self.last_combo = Some(Instant::now());
        }
        if let Cmd::SelectTool(tool) = cmd {
            if tool != self.tool {
                self.held = Some((vk, self.tool, false));
            }
        }
        Some(self.exec(cmd))
    }

    /// 松开工具键：按着它的时候用过（画了一笔、点了一下），就回到之前的工具。
    fn key_up(&mut self, vk: u16) {
        if let Some((held, before, used)) = self.held {
            if held == vk {
                self.held = None;
                if used {
                    self.exec(Cmd::SelectTool(before));
                }
            }
        }
    }

    /// 菜单里带勾的几项跟着当前状态打勾。
    fn sync_menu(&self) {
        let percent = (self.min_pressure * 100.0).round() as u8;
        let states = [
            (Cmd::ShowRgb, self.show_rgb),
            (Cmd::ShowHsv, self.show_hsv),
            (Cmd::ShowSwatches, self.show_swatches),
            (Cmd::PanelsRight, self.panels_right),
            (Cmd::PenPointer, self.tablet.is_none()),
            (Cmd::PenWintab, self.tablet.is_some()),
            (Cmd::MinPressure(0), percent == 0),
            (Cmd::MinPressure(2), percent == 2),
            (Cmd::MinPressure(5), percent == 5),
            (Cmd::MinPressure(10), percent == 10),
            (Cmd::Language(false), !lang::english()),
            (Cmd::Language(true), lang::english()),
        ];
        unsafe {
            let menu = GetMenu(self.hwnd);
            for (cmd, on) in states {
                if let Some(item) = MENUS.iter().flat_map(|m| m.2.iter()).find(|i| i.3 == cmd && i.0 != 0) {
                    CheckMenuItem(menu, item.0 as u32, (if on { MF_CHECKED } else { MF_UNCHECKED }).0);
                }
            }
        }
    }

    fn save_settings(&self) {
        let mut s = settings::Settings::default();
        s.set("stabilizer", self.stabilizer);
        s.set("panels", self.panels as usize);
        s.set("fill.tolerance", self.tolerance as usize);
        s.set("fill.gap", self.gap as usize);
        s.set("fill.merged", self.fill_merged as usize);
        s.set("show.rgb", self.show_rgb as usize);
        s.set("show.hsv", self.show_hsv as usize);
        s.set("show.swatches", self.show_swatches as usize);
        s.set("panels.right", self.panels_right as usize);
        s.set("pen.wintab", self.tablet.is_some() as usize);
        s.set("pen.min_pressure", (self.min_pressure * 100.0).round() as usize);
        s.set("lang", if lang::english() { "en" } else { "zh" });
        let hex: Vec<String> = self.swatches.iter().map(|c| format!("{:02X}{:02X}{:02X}", c[0], c[1], c[2])).collect();
        s.set("swatches", hex.join(","));
        self.keymap.save(&mut s);
        for tool in Tool::BRUSHES {
            let b = &self.brushes[tool as usize];
            let k = tool.key();
            s.set(&format!("{k}.size"), b.size);
            s.set(&format!("{k}.min_size"), b.min_size);
            s.set(&format!("{k}.density"), b.density);
            s.set(&format!("{k}.min_density"), b.min_density);
            s.set(&format!("{k}.hardness"), b.hardness);
        }
        s.save();
    }
}

fn wrap_angle(a: f32) -> f32 {
    let two_pi = std::f32::consts::TAU;
    let a = a % two_pi;
    if a > std::f32::consts::PI {
        a - two_pi
    } else if a < -std::f32::consts::PI {
        a + two_pi
    } else {
        a
    }
}

/// 在缓冲上画一圈一个像素宽的圆环：亮处画深色，暗处画浅色，这样在任何底色上都看得见。
fn draw_ring(buf: &mut [u32], stride: usize, origin: (i32, i32), vp: (i32, i32), c: (f32, f32), r: f32) {
    let steps = ((r * 7.0) as i32).clamp(16, 4096);
    for k in 0..steps {
        let a = k as f32 * std::f32::consts::TAU / steps as f32;
        let (x, y) = ((c.0 + r * a.cos()).round() as i32, (c.1 + r * a.sin()).round() as i32);
        if x < 0 || y < 0 || x >= vp.0 || y >= vp.1 {
            continue;
        }
        let px = &mut buf[(origin.1 + y) as usize * stride + (origin.0 + x) as usize];
        let luma = ((*px >> 16) & 0xFF) * 3 + ((*px >> 8) & 0xFF) * 6 + (*px & 0xFF);
        *px = if luma > 1280 { 0x0050_5050 } else { 0x00D0_D0D0 };
    }
}

/// 从 `a` 拖到 `b`（画布坐标）框出来的矩形：凡是碰到的像素都算进去。
fn marquee(a: (f32, f32), b: (f32, f32)) -> Rect {
    Rect {
        x0: a.0.min(b.0).floor() as i32,
        y0: a.1.min(b.1).floor() as i32,
        x1: a.0.max(b.0).ceil() as i32,
        y1: a.1.max(b.1).ceil() as i32,
    }
}

/// 在缓冲上画一条一个像素宽、黑白相间的虚线（视口内坐标），只画落在 `clip` 里的部分。
fn dashed(buf: &mut [u32], stride: usize, origin: (i32, i32), clip: Rect, a: (f32, f32), b: (f32, f32)) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let steps = dx.abs().max(dy.abs()).ceil().max(1.0);
    if !steps.is_finite() {
        return;
    }
    let (sx, sy) = (dx / steps, dy / steps);
    // 放得很大时线的绝大部分在屏幕外面，先把要走的那一段收到 `clip` 附近。
    let (mut k0, mut k1) = (0.0f32, steps);
    for (p, d, lo, hi) in [(a.0, sx, clip.x0, clip.x1), (a.1, sy, clip.y0, clip.y1)] {
        let (lo, hi) = (lo as f32 - 1.0, hi as f32 + 1.0);
        if d.abs() < 1e-6 {
            if p < lo || p > hi {
                return;
            }
        } else {
            let (ka, kb) = ((lo - p) / d, (hi - p) / d);
            k0 = k0.max(ka.min(kb));
            k1 = k1.min(ka.max(kb));
        }
    }
    let mut k = k0.floor().max(0.0);
    while k <= k1 {
        let (x, y) = ((a.0 + sx * k).floor() as i32, (a.1 + sy * k).floor() as i32);
        if x >= clip.x0 && x < clip.x1 && y >= clip.y0 && y < clip.y1 {
            let dark = ((x + y) >> 2) & 1 == 0;
            buf[(origin.1 + y) as usize * stride + (origin.0 + x) as usize] = if dark { 0x0010_1010 } else { 0x00FF_FFFF };
        }
        k += 1.0;
    }
}

/// 变换外框上的手柄：白底、深色边的小方块。
fn knob(buf: &mut [u32], stride: usize, origin: (i32, i32), clip: Rect, c: (f32, f32), half: i32) {
    let (cx, cy) = (c.0.round() as i32, c.1.round() as i32);
    for y in (cy - half).max(clip.y0)..(cy + half + 1).min(clip.y1) {
        for x in (cx - half).max(clip.x0)..(cx + half + 1).min(clip.x1) {
            let edge = (x - cx).abs() == half || (y - cy).abs() == half;
            buf[(origin.1 + y) as usize * stride + (origin.0 + x) as usize] = if edge { 0x0020_2020 } else { 0x00FF_FFFF };
        }
    }
}

/// 按扩展名读文件。返回文档、读入时要告诉用户的话（哪些内容没能照原样读进来），以及它是不是我们自己的格式。
fn load_any(path: &Path) -> std::io::Result<(Document, Vec<String>, bool)> {
    match extension(path).as_str() {
        "psd" => psd::load(path).map(|(doc, notes)| (doc, notes, false)),
        "ora" => ora::load(path).map(|(doc, notes)| (doc, notes, false)),
        _ => format::load(path).map(|doc| (doc, Vec::new(), true)),
    }
}

/// 打开别的格式的文件之后，告诉用户哪些内容没能照原样读进来。
fn tell_notes(hwnd: HWND, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    if let Some((font, scale)) = with_app(|app| (app.font, app.scale)) {
        dialogs::notes(hwnd, font, scale, notes);
    }
}

/// 设置文件里的色板：逗号隔开的六位十六进制颜色，认不出的跳过。
fn parse_swatches(text: &str) -> Vec<[u8; 3]> {
    let one = |t: &str| {
        let v = u32::from_str_radix(t.trim(), 16).ok().filter(|_| t.trim().len() == 6)?;
        Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
    };
    text.split(',').filter_map(one).take(MAX_SWATCHES).collect()
}

fn extension(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn open_filter() -> &'static str {
    tr(
        "能打开的文件 (*.zhibi;*.psd;*.ora)\0*.zhibi;*.psd;*.ora\0纸笔画布 (*.zhibi)\0*.zhibi\0\0",
        "Supported files (*.zhibi;*.psd;*.ora)\0*.zhibi;*.psd;*.ora\0Pen & Paper canvas (*.zhibi)\0*.zhibi\0\0",
    )
}

fn export_filter() -> &'static str {
    tr(
        "PNG 图像 (*.png)\0*.png\0Photoshop 文件 (*.psd)\0*.psd\0OpenRaster 文件 (*.ora)\0*.ora\0\0",
        "PNG image (*.png)\0*.png\0Photoshop file (*.psd)\0*.psd\0OpenRaster file (*.ora)\0*.ora\0\0",
    )
}

fn build_menu(keymap: &Keymap) -> Option<HMENU> {
    unsafe {
        let bar = CreateMenu().ok()?;
        for (zh, en, items) in MENUS {
            let title = tr(zh, en);
            let sub = CreatePopupMenu().ok()?;
            for (id, zh, en, cmd) in *items {
                let text = tr(zh, en);
                if *id == 0 {
                    let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                } else {
                    // 制表符后面的文字靠右显示：这个功能现在绑的键。
                    let text = keymap.hint(*cmd).map_or(text.to_string(), |key| format!("{text}\t{key}"));
                    let t = dialogs::wide(&text);
                    let _ = AppendMenuW(sub, MF_STRING, *id as usize, PCWSTR(t.as_ptr()));
                }
            }
            let t = dialogs::wide(title);
            let _ = AppendMenuW(bar, MF_POPUP, sub.0 as usize, PCWSTR(t.as_ptr()));
        }
        Some(bar)
    }
}

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(None)?;
        let class = w!("zhibi_main");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            // 程序图标：build.rs 把 assets/icon.ico 编进了资源，编号 1。没有编进去时用系统默认的。
            hIcon: LoadIconW(Some(hinst.into()), PCWSTR(std::ptr::without_provenance(1))).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let saved = settings::Settings::load();
        // 界面语言：设置文件里记着的优先，没有就跟系统的界面语言走（中文系统用中文，其余用英文）。
        let english = match saved.text("lang") {
            Some("en") => true,
            Some("zh") => false,
            _ => GetUserDefaultUILanguage() & 0x3FF != 0x04,
        };
        lang::set_english(english);
        let keymap = Keymap::load(&saved);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(dialogs::wide(tr("纸笔", "Pen & Paper")).as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1600,
            1000,
            None,
            build_menu(&keymap),
            Some(hinst.into()),
            None,
        )?;
        // 画布窗口不需要输入法。不关掉的话，中文输入法开着时字母快捷键会被它截走。
        // 需要打字的地方（图层改名、对话框）用独立的输入框，它们有自己的输入法状态。
        ImmAssociateContext(hwnd, HIMC::default());

        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        let mut brushes = Tool::BRUSHES.map(Tool::defaults);
        for tool in Tool::BRUSHES {
            let (b, k) = (&mut brushes[tool as usize], tool.key());
            b.size = saved.f32(&format!("{k}.size"), b.size).clamp(1.0, 500.0);
            b.min_size = saved.f32(&format!("{k}.min_size"), b.min_size).clamp(0.0, 1.0);
            b.density = saved.f32(&format!("{k}.density"), b.density).clamp(0.0, 1.0);
            b.min_density = saved.f32(&format!("{k}.min_density"), b.min_density).clamp(0.0, 1.0);
            b.hardness = saved.f32(&format!("{k}.hardness"), b.hardness).clamp(0.0, 1.0);
        }
        // 命令行带了文件名就直接打开它（双击文件、拖到程序图标上都走这里）。
        let arg = std::env::args_os().nth(1).map(PathBuf::from);
        let (doc, path, foreign, notes) = match arg.as_ref().map(|p| load_any(p)) {
            Some(Ok((doc, notes, native))) => (doc, arg, !native, notes),
            Some(Err(e)) => {
                dialogs::message(hwnd, &trf!("打不开这个文件：{e}", "Could not open the file: {e}"));
                (Document::new(3000, 2000), None, false, Vec::new())
            }
            None => (Document::new(3000, 2000), None, false, Vec::new()),
        };
        let view = View { zoom: 1.0, angle: 0.0, flip: false, center: (1500.0, 1000.0) };
        let empty = RECT::default();
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hwnd,
                scale,
                font: ui::make_font(scale),
                doc,
                path,
                foreign,
                display: Display::default(),
                view,
                back: None,
                layout: Layout { top: empty, left: empty, layers: empty, canvas: empty, items: Vec::new() },
                panels: saved.usize("panels", 1) != 0,
                tool: Tool::Pen,
                brushes,
                hsv: (0.0, 0.0, 0.0),
                background: [255, 255, 255],
                show_rgb: saved.usize("show.rgb", 0) != 0,
                show_hsv: saved.usize("show.hsv", 0) != 0,
                show_swatches: saved.usize("show.swatches", 0) != 0,
                panels_right: saved.usize("panels.right", 0) != 0,
                swatches: saved.text("swatches").map_or(SWATCHES.to_vec(), parse_swatches),
                stabilizer: saved.usize("stabilizer", 3).min(15),
                stroke: None,
                drag: Drag::None,
                drag_from: (0.0, 0.0),
                lasso: Vec::new(),
                combine: Combine::Replace,
                xform: None,
                tolerance: saved.usize("fill.tolerance", 24).min(255) as u8,
                gap: (saved.usize("fill.gap", 0) as u32).min(MAX_GAP),
                fill_merged: saved.usize("fill.merged", 1) != 0,
                grab: None,
                props_before: None,
                last_pos: (0.0, 0.0),
                ring: Rect::EMPTY,
                hovering: false,
                pen_near: false,
                // 上次用的是 Wintab 就接着用；这次打不开的话不声不响地退回 Windows 的笔输入。
                tablet: (saved.usize("pen.wintab", 0) != 0).then(|| wintab::Tablet::open(hwnd).ok()).flatten(),
                wintab_last: None,
                min_pressure: saved.usize("pen.min_pressure", 0).min(10) as f32 / 100.0,
                last_combo: None,
                keymap,
                held: None,
                pen_eraser: None,
                rename: None,
            })
        });
        let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
        with_app(|app| {
            app.sync_menu();
            let vp = app.vp();
            app.view.fit((app.doc.width, app.doc.height), vp);
            app.refresh_all();
        });
        tell_notes(hwnd, &notes);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // 改名输入框里的回车和 Esc 由我们处理：回车确认，Esc 放弃。
            if msg.message == WM_KEYDOWN {
                let editing = with_app(|app| app.rename.map(|r| r.1)).flatten();
                if editing == Some(msg.hwnd) && (msg.wParam.0 as u16 == VK_RETURN.0 || msg.wParam.0 as u16 == VK_ESCAPE.0) {
                    finish_rename(hwnd, msg.wParam.0 as u16 == VK_RETURN.0);
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

fn lparam_point(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32)
}

/// 笔样本在屏幕上的位置。优先用设备原生精度（himetric）换算出亚像素坐标，
/// 取不到设备范围时退回整数像素。
unsafe fn pen_screen_pos(info: &POINTER_PEN_INFO) -> (f32, f32) {
    let pi = &info.pointerInfo;
    let (mut dev, mut disp) = (RECT::default(), RECT::default());
    if GetPointerDeviceRects(pi.sourceDevice, &mut dev, &mut disp).is_ok() && dev.right > dev.left && dev.bottom > dev.top {
        let fx = (pi.ptHimetricLocationRaw.x - dev.left) as f32 / (dev.right - dev.left) as f32;
        let fy = (pi.ptHimetricLocationRaw.y - dev.top) as f32 / (dev.bottom - dev.top) as f32;
        (disp.left as f32 + fx * (disp.right - disp.left) as f32, disp.top as f32 + fy * (disp.bottom - disp.top) as f32)
    } else {
        (pi.ptPixelLocationRaw.x as f32 + 0.5, pi.ptPixelLocationRaw.y as f32 + 0.5)
    }
}

/// 处理笔消息。返回 false 表示这条消息不归画布管（笔在面板上），交给系统转成鼠标消息。
unsafe fn on_pen(hwnd: HWND, msg: u32, wp: WPARAM) -> bool {
    let id = (wp.0 & 0xFFFF) as u32;
    let mut kind = POINTER_INPUT_TYPE(0);
    if GetPointerType(id, &mut kind).is_err() || kind != PT_PEN {
        return false;
    }
    // 一条消息可能带着多个被系统合并的样本，全部取出，按时间先后处理（最新的排在最前）。
    let mut count = 64u32;
    let mut hist = [POINTER_PEN_INFO::default(); 64];
    if GetPointerPenInfoHistory(id, &mut count, Some(hist.as_mut_ptr())).is_err() {
        return false;
    }
    let n = (count as usize).min(hist.len());
    let mut origin = POINT::default();
    let _ = ClientToScreen(hwnd, &mut origin);

    // 一条消息里最新的样本排在最前，这里倒成时间先后。
    let samples: Vec<wintab::PenSample> = hist[..n]
        .iter()
        .rev()
        // 笔的标志位里 0x2 是"倒过来了"、0x4 是"橡皮头压着"：驱动把笔切成橡皮时也是这样报的。
        .map(|info| (pen_screen_pos(info), info.pressure as f32 / 1024.0, info.pointerInfo.pointerFlags.0 & 0x4 != 0, info.penFlags & 0x6 != 0))
        .collect();
    with_app(|app| !app.wintab_live() && app.pen(origin, &samples, msg == WM_POINTERUP)).unwrap_or(false)
}

/// Wintab 来了数据包：全部取出来，和 Windows 笔消息走同一条路。
unsafe fn on_wintab(hwnd: HWND) {
    let mut origin = POINT::default();
    let _ = ClientToScreen(hwnd, &mut origin);
    with_app(|app| {
        let mut samples = Vec::new();
        if let Some(tablet) = app.tablet.as_mut() {
            tablet.drain(&mut samples);
        }
        if !samples.is_empty() {
            app.wintab_last = Some(Instant::now());
            app.pen(origin, &samples, false);
        }
    });
}

unsafe fn on_mouse(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<Later> {
    let from_pen = GetMessageExtraInfo().0 & MI_WP_SIGNATURE_MASK == MI_WP_SIGNATURE;
    let (x, y) = lparam_point(lp);
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            SetCapture(hwnd);
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
        }
        _ => {}
    }
    with_app(|app| {
        let c = app.layout.canvas;
        let on_canvas = ui::contains(&c, x, y);
        let canvas_busy = app.stroke.is_some() || app.drag != Drag::None;
        if app.grab.is_some() || (!on_canvas && !canvas_busy) {
            if app.hovering && !canvas_busy {
                app.hovering = false;
                let old = app.ring;
                app.present(old, None);
            }
            return app.ui_mouse(msg, x, y);
        }
        // 画布上的笔已经由笔消息直接处理过了，转换出来的鼠标消息要丢掉，否则每一笔会画两遍。
        // 笔在感应范围内时，不带签名的鼠标消息也一并不理（见 `pen_near` 的说明）。
        if from_pen || app.pen_near || app.wintab_live() {
            return None;
        }
        let pos = (x as f32 + 0.5 - c.left as f32, y as f32 + 0.5 - c.top as f32);
        let contact = msg != WM_LBUTTONUP && wp.0 & 0x0001 != 0; // MK_LBUTTON
        let dirty = app.input(pos, 1.0, contact);
        if app.drag == Drag::None {
            app.canvas_changed(dirty);
        }
        None
    })
    .flatten()
}

// ---- 需要自己的消息循环的操作：都在放开应用状态之后做 ----

fn save_to(hwnd: HWND, path: PathBuf) -> bool {
    let result = with_app(|app| format::save(&app.doc, &path));
    match result {
        Some(Ok(())) => {
            with_app(|app| {
                app.doc.mark_saved();
                app.path = Some(path);
                app.foreign = false;
                app.refresh_ui();
            });
            true
        }
        Some(Err(e)) => {
            dialogs::message(hwnd, &trf!("保存失败：{e}", "Saving failed: {e}"));
            false
        }
        None => false,
    }
}

fn save(hwnd: HWND, ask_path: bool) -> bool {
    let (current, foreign) = with_app(|app| (app.path.clone(), app.foreign)).unwrap_or((None, false));
    let path = match (current, ask_path || foreign) {
        (Some(p), false) => Some(p),
        (current, _) => {
            let name = format!("{}.{}", stem(current.as_deref()), format::EXTENSION);
            dialogs::file_dialog(hwnd, true, tr("纸笔画布 (*.zhibi)\0*.zhibi\0\0", "Pen & Paper canvas (*.zhibi)\0*.zhibi\0\0"), format::EXTENSION, &name)
        }
    };
    path.is_some_and(|p| save_to(hwnd, p))
}

/// 文件名去掉扩展名的部分，没有文件名时是"未命名"。
fn stem(path: Option<&Path>) -> String {
    path.and_then(|p| p.file_stem()).map_or(tr("未命名", "Untitled").to_string(), |n| n.to_string_lossy().into_owned())
}

/// 要丢掉当前画布之前：有未保存的修改就先问。返回 false 表示用户取消了。
fn may_discard(hwnd: HWND) -> bool {
    if !with_app(|app| app.doc.is_modified()).unwrap_or(false) {
        return true;
    }
    match dialogs::ask_save(hwnd) {
        Some(true) => save(hwnd, false),
        Some(false) => true,
        None => false,
    }
}

fn replace_document(doc: Document, path: Option<PathBuf>, foreign: bool) {
    with_app(|app| {
        app.doc = doc;
        app.path = path;
        app.foreign = foreign;
        app.stroke = None;
        app.xform = None;
        app.drag = Drag::None;
        app.lasso.clear();
        app.display.invalidate_all();
        app.relayout();
        let vp = app.vp();
        app.view = View { zoom: 1.0, angle: 0.0, flip: false, center: (0.0, 0.0) };
        app.view.fit((app.doc.width, app.doc.height), vp);
        app.refresh_all();
    });
}

fn run_later(hwnd: HWND, later: Later) {
    match later {
        Later::Run(Cmd::New) => {
            if !may_discard(hwnd) {
                return;
            }
            let Some((font, scale)) = with_app(|app| (app.font, app.scale)) else { return };
            if let Some(n) = dialogs::new_canvas(hwnd, font, scale, MAX_SIDE) {
                let mut doc = Document::new(n.width, n.height);
                doc.dpi = n.dpi;
                replace_document(doc, None, false);
            }
        }
        Later::Run(Cmd::Open) => {
            if !may_discard(hwnd) {
                return;
            }
            if let Some(path) = dialogs::file_dialog(hwnd, false, open_filter(), format::EXTENSION, "") {
                match load_any(&path) {
                    Ok((doc, notes, native)) => {
                        replace_document(doc, Some(path), !native);
                        tell_notes(hwnd, &notes);
                    }
                    Err(e) => dialogs::message(hwnd, &trf!("打不开这个文件：{e}", "Could not open the file: {e}")),
                }
            }
        }
        Later::Run(Cmd::Save) => {
            save(hwnd, false);
        }
        Later::Run(Cmd::SaveAs) => {
            save(hwnd, true);
        }
        Later::Run(Cmd::Export) => {
            let name = format!("{}.png", with_app(|app| stem(app.path.as_deref())).unwrap_or_default());
            if let Some(path) = dialogs::file_dialog(hwnd, true, export_filter(), "png", &name) {
                // 导出成哪种格式看文件名的扩展名。
                let result = with_app(|app| match extension(&path).as_str() {
                    "psd" => psd::save(&app.doc, &path),
                    "ora" => ora::save(&app.doc, &path),
                    _ => app.doc.export_png(&path),
                });
                if let Some(Err(e)) = result {
                    dialogs::message(hwnd, &trf!("导出失败：{e}", "Export failed: {e}"));
                }
            }
        }
        Later::Run(Cmd::Exit) => unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        // 换语言：菜单整个重建，界面重排，标题跟着换。
        Later::Run(Cmd::Language(_)) => {
            let Some(keymap) = with_app(|app| app.keymap.clone()) else { return };
            unsafe {
                let old = GetMenu(hwnd);
                if let Some(menu) = build_menu(&keymap) {
                    let _ = SetMenu(hwnd, Some(menu));
                    let _ = DestroyMenu(old);
                }
            }
            with_app(|app| {
                app.sync_menu();
                app.refresh_all();
            });
        }
        Later::Run(Cmd::PenWintab) => match wintab::Tablet::open(hwnd) {
            Ok(tablet) => {
                with_app(|app| {
                    app.tablet = Some(tablet);
                    app.sync_menu();
                });
            }
            Err(why) => dialogs::message(hwnd, &trf!("{why}\n\n笔输入仍然用 Windows 的。", "{why}\n\nPen input stays on Windows Ink.")),
        },
        Later::Run(Cmd::Shortcuts) => {
            let Some((font, scale, keymap)) = with_app(|app| (app.font, app.scale, app.keymap.clone())) else { return };
            if let Some(changed) = dialogs::shortcuts(hwnd, font, scale, &keymap) {
                // 菜单上的快捷键提示跟着换。
                unsafe {
                    let old = GetMenu(hwnd);
                    if let Some(menu) = build_menu(&changed) {
                        let _ = SetMenu(hwnd, Some(menu));
                        let _ = DestroyMenu(old);
                    }
                }
                with_app(|app| {
                    app.keymap = changed;
                    app.sync_menu();
                    app.save_settings();
                });
            }
        }
        Later::Run(_) => {}
        Later::Popup(which, at) => {
            let mut at = at;
            unsafe {
                let _ = ClientToScreen(hwnd, &mut at);
            }
            let items: Vec<(u32, String, bool)> = match which {
                Popup::Stabilizer => {
                    let cur = with_app(|app| app.stabilizer).unwrap_or(0);
                    (0..=15).map(|n| (n as u32 + 1, if n == 0 { tr("0（关）", "0 (off)").to_string() } else { n.to_string() }, n == cur)).collect()
                }
                Popup::Blend => {
                    let cur = with_app(|app| app.doc.layers()[app.doc.active()].props.blend).unwrap_or(BlendMode::Normal);
                    BlendMode::ALL.iter().enumerate().map(|(i, m)| (i as u32 + 1, m.label().to_string(), *m == cur)).collect()
                }
            };
            let Some(choice) = dialogs::popup(hwnd, at, &items) else { return };
            with_app(|app| match which {
                Popup::Stabilizer => {
                    app.stabilizer = (choice - 1) as usize;
                    app.refresh_ui();
                }
                Popup::Blend => {
                    let index = app.doc.active();
                    let before = app.doc.layers()[index].props.clone();
                    app.doc.set_props_live(index, LayerProps { blend: BlendMode::ALL[(choice - 1) as usize], ..before.clone() });
                    app.doc.commit_props(index, before);
                    app.document_changed();
                }
            });
        }
        Later::Rename(index, rect) => unsafe {
            let Some((name, font)) = with_app(|app| (app.doc.layers()[index].props.name.clone(), app.font)) else { return };
            let text = dialogs::wide(&name);
            let Ok(edit) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                PCWSTR(text.as_ptr()),
                WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                Some(hwnd),
                None,
                None,
                None,
            ) else {
                return;
            };
            SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            SendMessageW(edit, dialogs::EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            with_app(|app| app.rename = Some((index, edit)));
            let _ = SetFocus(Some(edit));
        },
    }
}

/// 结束原地改名。`keep` 为 false 时放弃输入。
fn finish_rename(hwnd: HWND, keep: bool) {
    let Some((index, edit)) = with_app(|app| app.rename.take()).flatten() else { return };
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(edit, &mut buf);
        let name = String::from_utf16_lossy(&buf[..n.max(0) as usize]).trim().to_string();
        let _ = DestroyWindow(edit);
        let _ = SetFocus(Some(hwnd));
        if keep && !name.is_empty() {
            with_app(|app| {
                if let Some(layer) = app.doc.layers().get(index) {
                    let before = layer.props.clone();
                    app.doc.set_props_live(index, LayerProps { name, ..before.clone() });
                    app.doc.commit_props(index, before);
                }
                app.refresh_ui();
            });
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TABLET_QUERYSYSTEMGESTURESTATUS => LRESULT(TABLET_DISABLE_ALL),
            WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP => {
                if on_pen(hwnd, msg, wp) {
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            WM_POINTERLEAVE => {
                with_app(|app| {
                    app.pen_near = false;
                    if app.stroke.is_none() && app.hovering {
                        app.hovering = false;
                        let old = app.ring;
                        app.present(old, None);
                    }
                });
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            wintab::WT_PACKET => {
                on_wintab(hwnd);
                LRESULT(0)
            }
            wintab::WT_PROXIMITY => {
                // 笔离开了数位板的感应范围：画到一半的一笔就此结束。
                if lp.0 & 0xFFFF == 0 {
                    with_app(|app| {
                        app.pen_near = false;
                        app.wintab_last = None;
                        let dirty = app.input(app.last_pos, 0.0, false);
                        app.hovering = false;
                        app.canvas_changed(dirty);
                    });
                }
                LRESULT(0)
            }
            WM_ACTIVATE => {
                with_app(|app| {
                    if let Some(tablet) = app.tablet.as_ref() {
                        tablet.activate(wp.0 & 0xFFFF != 0);
                    }
                });
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP | WM_MOUSEMOVE => {
                if matches!(msg, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK) {
                    finish_rename(hwnd, true);
                }
                if let Some(later) = on_mouse(hwnd, msg, wp, lp) {
                    run_later(hwnd, later);
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
                let mut pt = POINT { x: lparam_point(lp).0, y: lparam_point(lp).1 };
                let _ = ScreenToClient(hwnd, &mut pt);
                with_app(|app| {
                    let c = app.layout.canvas;
                    if ui::contains(&c, pt.x, pt.y) {
                        let z = app.view.zoom * WHEEL_STEP.powf(delta);
                        app.zoom_to(z, ((pt.x - c.left) as f32, (pt.y - c.top) as f32));
                    }
                });
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = wp.0 as u16;
                // Alt 和 Alt+空格是我们的导航修饰键，不让系统拿去打开窗口菜单。
                if msg == WM_SYSKEYDOWN && (vk == VK_MENU.0 || vk == VK_SPACE.0) {
                    return LRESULT(0);
                }
                match with_app(|app| app.key(vk)).flatten() {
                    Some(later) => {
                        if let Some(later) = later {
                            run_later(hwnd, later);
                        }
                        LRESULT(0)
                    }
                    None => DefWindowProcW(hwnd, msg, wp, lp),
                }
            }
            WM_KEYUP => {
                with_app(|app| app.key_up(wp.0 as u16));
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_SYSKEYUP if wp.0 as u16 == VK_MENU.0 => LRESULT(0),
            WM_SYSCOMMAND if (wp.0 & 0xFFF0) == SC_KEYMENU as usize => LRESULT(0),
            WM_COMMAND => {
                let (id, code) = ((wp.0 & 0xFFFF) as u16, ((wp.0 >> 16) & 0xFFFF) as u32);
                if lp.0 != 0 {
                    // 来自子控件：改名输入框失去焦点时确认改名。
                    if code == EN_KILLFOCUS {
                        finish_rename(hwnd, true);
                    }
                    return LRESULT(0);
                }
                let cmd = MENUS.iter().flat_map(|m| m.2.iter()).find(|i| i.0 == id && id != 0).map(|i| i.3);
                if let Some(later) = cmd.and_then(|c| with_app(|app| app.exec(c)).flatten()) {
                    run_later(hwnd, later);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let (w, h) = lparam_point(lp);
                with_app(|app| {
                    app.resize(w, h);
                    app.refresh_all();
                });
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                with_app(|app| app.paint(hdc, &ps.rcPaint));
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_CLOSE => {
                if may_discard(hwnd) {
                    with_app(|app| app.save_settings());
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
