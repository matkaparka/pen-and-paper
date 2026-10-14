//! 注入前的安全检查：合成笔输入会落到屏幕上那个位置的任何窗口里，
//! 所以每次落笔前都确认目标位置确实是预期的程序，并且它在前台。

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_ESCAPE, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetForegroundWindow, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, WindowFromPoint,
    GA_ROOT,
};
use windows::core::{BOOL, PWSTR};

use crate::util::Rect;

pub fn exe_of(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string())
    }
}

pub fn title_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn root_at(x: i32, y: i32) -> HWND {
    unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) }
}

/// 检查区域的四角和中心是否都属于 expect_exe 的同一个顶层窗口，返回该窗口。
pub fn target_window(roi: Rect, expect_exe: &str) -> Result<HWND, String> {
    let probes = [
        (roi.x + roi.w / 2, roi.y + roi.h / 2),
        (roi.x + 2, roi.y + 2),
        (roi.x + roi.w - 3, roi.y + 2),
        (roi.x + 2, roi.y + roi.h - 3),
        (roi.x + roi.w - 3, roi.y + roi.h - 3),
    ];
    let root = root_at(probes[0].0, probes[0].1);
    for (x, y) in probes {
        let r = root_at(x, y);
        let exe = exe_of(r).unwrap_or_else(|| "<未知>".into());
        if !exe.eq_ignore_ascii_case(expect_exe) {
            return Err(format!("({x},{y}) 处的窗口属于 {exe}（标题 \"{}\"），不是预期的 {expect_exe}", title_of(r)));
        }
        if r != root {
            return Err(format!("({x},{y}) 处与区域中心不是同一个顶层窗口"));
        }
    }
    Ok(root)
}

/// 单点版本：(x, y) 处必须是 expect_exe 的窗口，返回它的顶层窗口。
pub fn window_at(x: i32, y: i32, expect_exe: &str) -> Result<HWND, String> {
    let root = root_at(x, y);
    let exe = exe_of(root).unwrap_or_else(|| "<未知>".into());
    if !exe.eq_ignore_ascii_case(expect_exe) {
        return Err(format!("({x},{y}) 处的窗口属于 {exe}（标题 \"{}\"），不是预期的 {expect_exe}", title_of(root)));
    }
    Ok(root)
}

/// 键盘输入发给前台窗口，所以先确认前台窗口属于 expect_exe。
pub fn foreground_is(expect_exe: &str) -> Result<(), String> {
    let fg = unsafe { GetForegroundWindow() };
    let exe = exe_of(fg).unwrap_or_else(|| "<未知>".into());
    if exe.eq_ignore_ascii_case(expect_exe) {
        Ok(())
    } else {
        Err(format!("前台窗口属于 {exe}（标题 \"{}\"），不是预期的 {expect_exe}", title_of(fg)))
    }
}

pub fn bring_to_front(hwnd: HWND) -> Result<(), String> {
    unsafe {
        if GetForegroundWindow() != hwnd {
            let _ = SetForegroundWindow(hwnd);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        if GetForegroundWindow() != hwnd {
            // 系统只允许"刚收到过输入的进程"切前台。按一下 Alt 再试，这是公开的常用做法。
            let alt = |up: bool| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_MENU,
                        dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                        ..Default::default()
                    },
                },
            };
            SendInput(&[alt(false), alt(true)], std::mem::size_of::<INPUT>() as i32);
            let _ = SetForegroundWindow(hwnd);
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        if GetForegroundWindow() != hwnd {
            return Err("目标窗口不在前台，且系统拒绝了切换前台的请求；请先手动点一下目标窗口".into());
        }
    }
    Ok(())
}

pub fn is_foreground(hwnd: HWND) -> bool {
    unsafe { GetForegroundWindow() == hwnd }
}

pub fn esc_pressed() -> bool {
    unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000 != 0 }
}

pub struct WinInfo {
    pub hwnd: HWND,
    pub title: String,
    pub exe: String,
    pub rect: Rect,
    pub foreground: bool,
}

pub fn list_windows() -> Vec<WinInfo> {
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<WinInfo>);
        if IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() {
            let title = title_of(hwnd);
            let mut rc = RECT::default();
            if !title.is_empty() && GetWindowRect(hwnd, &mut rc).is_ok() && rc.right > rc.left {
                out.push(WinInfo {
                    hwnd,
                    title,
                    exe: exe_of(hwnd).unwrap_or_default(),
                    rect: Rect { x: rc.left, y: rc.top, w: rc.right - rc.left, h: rc.bottom - rc.top },
                    foreground: GetForegroundWindow() == hwnd,
                });
            }
        }
        BOOL(1)
    }
    let mut out: Vec<WinInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}
