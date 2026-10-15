// SPDX-License-Identifier: GPL-3.0-or-later
//! 对话框和弹出菜单。它们都有自己的消息循环，调用期间不能占着应用状态。

use std::cell::Cell;
use std::path::PathBuf;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, SetBkMode, HBRUSH, HDC, HFONT, TRANSPARENT, WHITE_BRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_FILEMUSTEXIST, OFN_OVERWRITEPROMPT, OPENFILENAMEW,
};
use windows::Win32::UI::Input::Ime::{ImmAssociateContext, ImmAssociateContextEx, HIMC, IACE_CHILDREN};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, SetFocus, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_MENU, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::keys::{Key, Keymap, ACTIONS};
use zhibi_core::lang::tr;
use zhibi_core::trf;

/// 输入框的"选中一段文字"消息。
pub const EM_SETSEL: u32 = 0x00B1;

/// 程序的名字，跟着界面语言。
pub fn app_name() -> &'static str {
    tr("纸笔", "Pen & Paper")
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 系统的打开 / 保存文件对话框。`filter` 形如 "说明\0*.ext\0\0"。
pub fn file_dialog(owner: HWND, save: bool, filter: &str, ext: &str, name: &str) -> Option<PathBuf> {
    let mut file = [0u16; 1024];
    for (d, s) in file.iter_mut().zip(name.encode_utf16()) {
        *d = s;
    }
    let filter: Vec<u16> = filter.encode_utf16().collect();
    let ext = wide(ext);
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrDefExt: PCWSTR(ext.as_ptr()),
        Flags: if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST },
        ..Default::default()
    };
    let ok = unsafe {
        if save {
            GetSaveFileNameW(&mut ofn)
        } else {
            GetOpenFileNameW(&mut ofn)
        }
    };
    if !ok.as_bool() {
        return None;
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    Some(PathBuf::from(String::from_utf16_lossy(&file[..len])))
}

/// 在屏幕坐标 `at` 处弹出一个菜单，返回选中项的编号；没选返回 None。
/// `items` 是（编号，文字，是否打勾）。
/// 编号为 0 的一项是分隔线。
pub fn popup(owner: HWND, at: POINT, items: &[(u32, String, bool)]) -> Option<u32> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (id, text, checked) in items {
            if *id == 0 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                continue;
            }
            let t = wide(text);
            let flags = if *checked { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, flags, *id as usize, PCWSTR(t.as_ptr()));
        }
        let r = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTALIGN | TPM_TOPALIGN, at.x, at.y, None, owner, None);
        let _ = DestroyMenu(menu);
        (r.0 != 0).then_some(r.0 as u32)
    }
}

/// 询问是否保存。返回 Some(true) 保存，Some(false) 不保存，None 取消。
pub fn ask_save(owner: HWND) -> Option<bool> {
    let r = unsafe {
        let (text, title) = (wide(tr("当前画布有未保存的修改。要先保存吗？", "The canvas has unsaved changes. Save them first?")), wide(app_name()));
        MessageBoxW(Some(owner), PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_YESNOCANCEL | MB_ICONQUESTION)
    };
    match r {
        IDYES => Some(true),
        IDNO => Some(false),
        _ => None,
    }
}

pub fn message(owner: HWND, text: &str) {
    let t = wide(text);
    unsafe {
        let title = wide(app_name());
        MessageBoxW(Some(owner), PCWSTR(t.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONWARNING);
    }
}

// ---- 新建画布 ----

pub struct NewCanvas {
    pub width: i32,
    pub height: i32,
    pub dpi: u32,
}

/// 预设：中文名、英文名、宽、高、分辨率。
const PRESETS: &[(&str, &str, i32, i32, u32)] = &[
    ("3000 × 2000", "3000 × 2000", 3000, 2000, 300),
    ("1920 × 1080（屏幕）", "1920 × 1080 (screen)", 1920, 1080, 96),
    ("2480 × 3508（A4 竖，300 dpi）", "2480 × 3508 (A4 portrait, 300 dpi)", 2480, 3508, 300),
    ("3508 × 2480（A4 横，300 dpi）", "3508 × 2480 (A4 landscape, 300 dpi)", 3508, 2480, 300),
    ("4096 × 4096", "4096 × 4096", 4096, 4096, 300),
];

const ID_PRESET: i32 = 100;
const ID_WIDTH: i32 = 101;
const ID_HEIGHT: i32 = 102;
const ID_DPI: i32 = 103;
const ID_KEYS: i32 = 110;
const ID_FACTORY: i32 = 111;

thread_local! {
    /// 对话框的结束状态：0 进行中，1 确定，2 取消，3 按了"恢复出厂键位"
    static DONE: Cell<u8> = const { Cell::new(0) };
}

unsafe fn child(parent: HWND, class: PCWSTR, text: &str, style: WINDOW_STYLE, r: RECT, id: i32, font: HFONT) -> HWND {
    let t = wide(text);
    let ex = if class == w!("EDIT") { WS_EX_CLIENTEDGE } else { WINDOW_EX_STYLE(0) };
    let h = CreateWindowExW(
        ex,
        class,
        PCWSTR(t.as_ptr()),
        WS_CHILD | WS_VISIBLE | style,
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
        Some(parent),
        Some(HMENU(id as isize as *mut _)),
        None,
        None,
    )
    .unwrap_or_default();
    SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    h
}

unsafe fn set_int(dlg: HWND, id: i32, v: i64) {
    let t = wide(&v.to_string());
    let _ = SetDlgItemTextW(dlg, id, PCWSTR(t.as_ptr()));
}

unsafe fn get_int(dlg: HWND, id: i32) -> Option<i64> {
    let mut buf = [0u16; 32];
    let n = GetDlgItemTextW(dlg, id, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize]).trim().parse().ok()
}

extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let (id, code) = ((wp.0 & 0xFFFF) as i32, ((wp.0 >> 16) & 0xFFFF) as u32);
                if id == IDOK.0 {
                    DONE.set(1);
                } else if id == IDCANCEL.0 {
                    DONE.set(2);
                } else if id == ID_FACTORY {
                    DONE.set(3);
                } else if id == ID_PRESET && code == CBN_SELCHANGE {
                    let sel = SendDlgItemMessageW(hwnd, ID_PRESET, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                    if let Some(p) = PRESETS.get(sel as usize) {
                        set_int(hwnd, ID_WIDTH, p.2 as i64);
                        set_int(hwnd, ID_HEIGHT, p.3 as i64);
                        set_int(hwnd, ID_DPI, p.4 as i64);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                DONE.set(2);
                LRESULT(0)
            }
            // 说明文字的底色跟着对话框用白的，不然它们各自带一块灰底。
            WM_CTLCOLORSTATIC => {
                SetBkMode(HDC(wp.0 as *mut _), TRANSPARENT);
                LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// 新建画布对话框：预设、宽、高、分辨率四项，回车即建（规格 F-001）。
pub fn new_canvas(owner: HWND, font: HFONT, scale: f32, max_side: i32) -> Option<NewCanvas> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let class = w!("zhibi_dialog");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(dialog_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc); // 重复注册会失败，无妨

        let px = |v: i32| (v as f32 * scale) as i32;
        let (cw, ch) = (px(340), px(210));
        let mut wr = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let _ = AdjustWindowRectEx(&mut wr, style, false, WS_EX_DLGMODALFRAME);
        let mut or = RECT::default();
        let _ = GetWindowRect(owner, &mut or);
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class,
            PCWSTR(wide(tr("新建画布", "New Canvas")).as_ptr()),
            style,
            (or.left + or.right - ww) / 2,
            (or.top + or.bottom - wh) / 2,
            ww,
            wh,
            Some(owner),
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;

        let row = |i: i32| px(16 + i * 38);
        let label = |i: i32, text: &str| {
            child(dlg, w!("STATIC"), text, WINDOW_STYLE(0), RECT { left: px(16), top: row(i) + px(4), right: px(128), bottom: row(i) + px(28) }, -1, font)
        };
        let field = |i: i32| RECT { left: px(132), top: row(i), right: px(324), bottom: row(i) + px(28) };
        label(0, tr("预设", "Preset"));
        let combo = child(
            dlg,
            w!("COMBOBOX"),
            "",
            WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
            RECT { bottom: row(0) + px(240), ..field(0) },
            ID_PRESET,
            font,
        );
        for p in PRESETS {
            let t = wide(tr(p.0, p.1));
            SendMessageW(combo, CB_ADDSTRING, Some(WPARAM(0)), Some(LPARAM(t.as_ptr() as isize)));
        }
        SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(0)), Some(LPARAM(0)));
        let number = WINDOW_STYLE(ES_NUMBER as u32) | WS_TABSTOP;
        label(1, tr("宽（像素）", "Width (px)"));
        let width = child(dlg, w!("EDIT"), "", number, field(1), ID_WIDTH, font);
        label(2, tr("高（像素）", "Height (px)"));
        child(dlg, w!("EDIT"), "", number, field(2), ID_HEIGHT, font);
        label(3, tr("分辨率（dpi）", "Resolution (dpi)"));
        child(dlg, w!("EDIT"), "", number, field(3), ID_DPI, font);
        set_int(dlg, ID_WIDTH, PRESETS[0].2 as i64);
        set_int(dlg, ID_HEIGHT, PRESETS[0].3 as i64);
        set_int(dlg, ID_DPI, PRESETS[0].4 as i64);
        let by = px(168);
        child(dlg, w!("BUTTON"), tr("确定", "OK"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, RECT { left: px(148), top: by, right: px(232), bottom: by + px(30) }, IDOK.0, font);
        child(dlg, w!("BUTTON"), tr("取消", "Cancel"), WS_TABSTOP, RECT { left: px(240), top: by, right: px(324), bottom: by + px(30) }, IDCANCEL.0, font);

        DONE.set(0);
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetFocus(Some(width));
        SendMessageW(width, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));

        let mut result = None;
        let mut msg = MSG::default();
        loop {
            match DONE.get() {
                1 => {
                    let (w, h, d) = (get_int(dlg, ID_WIDTH), get_int(dlg, ID_HEIGHT), get_int(dlg, ID_DPI));
                    match (w, h) {
                        (Some(w), Some(h)) if (1..=max_side as i64).contains(&w) && (1..=max_side as i64).contains(&h) => {
                            result = Some(NewCanvas { width: w as i32, height: h as i32, dpi: d.unwrap_or(300).clamp(1, 9999) as u32 });
                            break;
                        }
                        _ => {
                            DONE.set(0);
                            message(dlg, &trf!("宽和高要在 1 到 {max_side} 之间。", "Width and height must be between 1 and {max_side}."));
                        }
                    }
                }
                2 => break,
                _ => {}
            }
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                PostQuitMessage(0);
                break;
            }
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = EnableWindow(owner, true);
        let _ = DestroyWindow(dlg);
        let _ = SetForegroundWindow(owner);
        result
    }
}

// ---- 快捷键 ----

/// 把键位表重新填进列表框，保持原来的选中行和滚动位置。
unsafe fn fill_keys(list: HWND, map: &Keymap) {
    let sel = SendMessageW(list, LB_GETCURSEL, None, None).0.max(0);
    let top = SendMessageW(list, LB_GETTOPINDEX, None, None).0;
    SendMessageW(list, WM_SETREDRAW, Some(WPARAM(0)), None);
    SendMessageW(list, LB_RESETCONTENT, None, None);
    for (i, a) in ACTIONS.iter().enumerate() {
        let row = wide(&format!("{}\t{}", a.label(), map.get(i).map_or("—".to_string(), |k| k.name())));
        SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(row.as_ptr() as isize)));
    }
    SendMessageW(list, LB_SETTOPINDEX, Some(WPARAM(top as usize)), None);
    SendMessageW(list, LB_SETCURSEL, Some(WPARAM(sel as usize)), None);
    SendMessageW(list, WM_SETREDRAW, Some(WPARAM(1)), None);
    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(list), None, true);
}

/// 快捷键对话框：点一个功能，然后直接按想用的键（规格 F-153）。
/// 返回改过之后的键位；什么都没改返回 None。
pub fn shortcuts(owner: HWND, font: HFONT, scale: f32, current: &Keymap) -> Option<Keymap> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let class = w!("zhibi_dialog");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(dialog_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc); // 重复注册会失败，无妨

        let px = |v: i32| (v as f32 * scale) as i32;
        let (cw, ch) = (px(440), px(560));
        let mut wr = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let _ = AdjustWindowRectEx(&mut wr, style, false, WS_EX_DLGMODALFRAME);
        let mut or = RECT::default();
        let _ = GetWindowRect(owner, &mut or);
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class,
            PCWSTR(wide(tr("快捷键", "Keyboard Shortcuts")).as_ptr()),
            style,
            (or.left + or.right - ww) / 2,
            (or.top + or.bottom - wh) / 2,
            ww,
            wh,
            Some(owner),
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;

        let line = |i: i32, text: &str| {
            child(dlg, w!("STATIC"), text, WINDOW_STYLE(0), RECT { left: px(16), top: px(12 + i * 22), right: cw - px(16), bottom: px(34 + i * 22) }, -1, font)
        };
        line(0, tr("点一个功能，然后直接按下想用的键。", "Click a command, then press the key you want."));
        line(1, tr("退格键：让这个功能不绑键。Esc：关闭。", "Backspace: remove the key. Esc: close."));
        let list = child(
            dlg,
            w!("LISTBOX"),
            "",
            WINDOW_STYLE((LBS_NOTIFY | LBS_USETABSTOPS | LBS_NOINTEGRALHEIGHT) as u32) | WS_VSCROLL | WS_BORDER,
            RECT { left: px(16), top: px(64), right: cw - px(16), bottom: ch - px(56) },
            ID_KEYS,
            font,
        );
        // 第二列（键名）的位置，单位是对话框单位。
        let stop = [130i32];
        SendMessageW(list, LB_SETTABSTOPS, Some(WPARAM(1)), Some(LPARAM(stop.as_ptr() as isize)));
        let by = ch - px(44);
        child(dlg, w!("BUTTON"), tr("恢复出厂键位", "Restore Defaults"), WINDOW_STYLE(0), RECT { left: px(16), top: by, right: px(150), bottom: by + px(30) }, ID_FACTORY, font);
        child(dlg, w!("BUTTON"), tr("关闭", "Close"), WINDOW_STYLE(0), RECT { left: cw - px(100), top: by, right: cw - px(16), bottom: by + px(30) }, IDCANCEL.0, font);

        // 中文输入法开着时，字母键会先被输入法截走，到不了这里，绑不上键。
        // 这个对话框里没有要打字的地方，整个关掉输入法（主窗口也是这么做的）。
        let _ = ImmAssociateContextEx(dlg, HIMC::default(), IACE_CHILDREN);
        ImmAssociateContext(dlg, HIMC::default());
        let mut map = current.clone();
        fill_keys(list, &map);
        DONE.set(0);
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetFocus(Some(list));

        let down = |vk: u16| GetKeyState(vk as i32) < 0;
        let mut msg = MSG::default();
        loop {
            match DONE.get() {
                3 => {
                    map = Keymap::factory();
                    fill_keys(list, &map);
                    let _ = SetFocus(Some(list));
                    DONE.set(0);
                }
                0 => {}
                _ => break,
            }
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                PostQuitMessage(0);
                break;
            }
            // 按键在这里截下来：除了上下键留给列表移动选中行，别的键都当成"要绑的键"，不交给控件。
            let ours = msg.hwnd == dlg || IsChild(dlg, msg.hwnd).as_bool();
            if ours && (msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN) {
                let vk = msg.wParam.0 as u16;
                let sel = SendMessageW(list, LB_GETCURSEL, None, None).0;
                if vk == VK_ESCAPE.0 {
                    break;
                }
                let key = Key { vk, ctrl: down(VK_CONTROL.0), shift: down(VK_SHIFT.0), alt: down(VK_MENU.0) };
                let change = if vk == VK_BACK.0 {
                    Some(None)
                } else if key.bindable() {
                    Some(Some(key))
                } else {
                    None
                };
                if let (Some(change), true) = (change, sel >= 0) {
                    map.set(sel as usize, change);
                    fill_keys(list, &map);
                    continue;
                }
                if vk != VK_UP.0 && vk != VK_DOWN.0 {
                    continue;
                }
            }
            // 单按 Alt 松开时系统会去激活窗口菜单，这里用不着。
            if ours && msg.message == WM_SYSKEYUP {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = EnableWindow(owner, true);
        let _ = DestroyWindow(dlg);
        let _ = SetForegroundWindow(owner);
        (map != *current).then_some(map)
    }
}

// ---- 打开文件后的说明 ----

/// 打开别的格式的文件之后，把没能照原样读进来的内容列给用户看。
/// 条目可能很多、很长，所以放在一个能滚动、能复制的文本框里，而不是系统的消息框。
pub fn notes(owner: HWND, font: HFONT, scale: f32, notes: &[String]) {
    unsafe {
        let Ok(hinst) = GetModuleHandleW(None) else { return };
        let class = w!("zhibi_dialog");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(dialog_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc); // 重复注册会失败，无妨

        let px = |v: i32| (v as f32 * scale) as i32;
        let (cw, ch) = (px(560), px(340));
        let mut wr = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let _ = AdjustWindowRectEx(&mut wr, style, false, WS_EX_DLGMODALFRAME);
        let mut or = RECT::default();
        let _ = GetWindowRect(owner, &mut or);
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        let Ok(dlg) = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class,
            PCWSTR(wide(tr("文件已经打开", "File Opened")).as_ptr()),
            style,
            (or.left + or.right - ww) / 2,
            (or.top + or.bottom - wh) / 2,
            ww,
            wh,
            Some(owner),
            None,
            Some(hinst.into()),
            None,
        ) else {
            return;
        };
        let head = RECT { left: px(16), top: px(14), right: cw - px(16), bottom: px(38) };
        child(dlg, w!("STATIC"), tr("有些内容没能照原样读进来：", "Some content could not be read as it was:"), WINDOW_STYLE(0), head, -1, font);
        // 文本框里换行要用回车加换行；条目之间空一行。
        let text = notes.join("\r\n\r\n");
        let body = RECT { left: px(16), top: px(44), right: cw - px(16), bottom: ch - px(58) };
        let style = WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32) | WS_VSCROLL;
        child(dlg, w!("EDIT"), &text, style, body, -1, font);
        let by = ch - px(44);
        let ok = RECT { left: cw - px(100), top: by, right: cw - px(16), bottom: by + px(30) };
        let button = child(dlg, w!("BUTTON"), tr("确定", "OK"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, ok, IDOK.0, font);

        DONE.set(0);
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetFocus(Some(button));
        let mut msg = MSG::default();
        while DONE.get() == 0 {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                PostQuitMessage(0);
                break;
            }
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = EnableWindow(owner, true);
        let _ = DestroyWindow(dlg);
        let _ = SetForegroundWindow(owner);
    }
}
