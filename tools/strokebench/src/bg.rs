//! 后台驱动：不把窗口切到前台、不动系统指针，直接给目标窗口发鼠标、键盘和菜单消息，
//! 再用 PrintWindow 取它自己画出来的画面。用户正在用电脑时，用它来检查我们自己程序的界面行为。
//!
//! 它走的不是真实的输入通路：没有压感，修饰键也按不下去（程序查询的是真实键盘状态）。
//! 所以只用来验证"点了这里、拖了这一段之后画面对不对"，延迟一类的指标仍然要用 `run`。

use std::path::Path;
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, UpdateWindow, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, PostMessageW, SendMessageW, WM_COMMAND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_CLOSE, WM_MOUSEMOVE, WM_MOUSEWHEEL,
};

use crate::guard;
use crate::util::write_png;

const MK_LBUTTON: usize = 0x0001;
const PW_CLIENTONLY: u32 = 0x1;
const PW_RENDERFULLCONTENT: u32 = 0x2;

fn point(text: &str) -> Result<(i32, i32), String> {
    let v: Vec<i32> = text.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    if v.len() == 2 {
        Ok((v[0], v[1]))
    } else {
        Err(format!("坐标要写成 X,Y：{text}"))
    }
}

fn lparam(p: (i32, i32)) -> LPARAM {
    LPARAM(((p.1 as u16 as isize) << 16) | (p.0 as u16 as isize))
}

fn send(hwnd: HWND, msg: u32, wp: usize, lp: LPARAM) {
    unsafe {
        SendMessageW(hwnd, msg, Some(WPARAM(wp)), Some(lp));
    }
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    (rc.right, rc.bottom)
}

/// 把窗口客户区自己画的内容存成 PNG。窗口被别的窗口挡住也能取到（最小化时不行）。
fn grab(hwnd: HWND, path: &Path, div: usize) -> Result<(), String> {
    let (w, h) = client_size(hwnd);
    if w <= 0 || h <= 0 {
        return Err("窗口没有客户区（可能最小化了）".into());
    }
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).map_err(|e| e.to_string())?;
        let old = SelectObject(dc, bitmap.into());
        let ok = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_CLIENTONLY | PW_RENDERFULLCONTENT)).as_bool();
        let bgra = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        if !ok {
            return Err("PrintWindow 失败".into());
        }
        write_png(path, &bgra, w as usize, h as usize, div).map_err(|e| e.to_string())
    }
}

fn vk(name: &str) -> Result<usize, String> {
    let n = name.to_ascii_lowercase();
    Ok(match n.as_str() {
        "enter" => 0x0D,
        "esc" => 0x1B,
        "tab" => 0x09,
        "space" => 0x20,
        "delete" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "[" => 0xDB,
        "]" => 0xDD,
        _ if n.len() == 1 && n.as_bytes()[0].is_ascii_alphanumeric() => n.to_ascii_uppercase().as_bytes()[0] as usize,
        _ if n.starts_with('f') && n[1..].parse::<usize>().is_ok_and(|k| (1..=12).contains(&k)) => 0x6F + n[1..].parse::<usize>().unwrap(),
        _ => return Err(format!("不认识的键：{name}")),
    })
}

/// 从 `a` 拖到 `b`，中间走 `steps` 步。`timed` 为 true 时每一步之后都让窗口立刻重画，并记下这一步用了多久。
fn drag(hwnd: HWND, a: (i32, i32), b: (i32, i32), steps: i32, timed: bool) -> Vec<f64> {
    let mut times = Vec::new();
    send(hwnd, WM_MOUSEMOVE, 0, lparam(a));
    send(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam(a));
    for k in 1..=steps.max(1) {
        let t = k as f64 / steps.max(1) as f64;
        let p = ((a.0 as f64 + (b.0 - a.0) as f64 * t).round() as i32, (a.1 as f64 + (b.1 - a.1) as f64 * t).round() as i32);
        let start = Instant::now();
        send(hwnd, WM_MOUSEMOVE, MK_LBUTTON, lparam(p));
        if timed {
            unsafe {
                let _ = UpdateWindow(hwnd);
            }
            times.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    send(hwnd, WM_LBUTTONUP, 0, lparam(b));
    times
}

fn post(hwnd: HWND, msg: u32, wp: usize, lp: LPARAM) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(wp), lp);
    }
}

/// `strokebench bg --exe NAME.exe [--title 标题里的一段] --do "步骤;步骤;…"`
pub fn run(exe: &str, title: Option<&str>, script: &str) -> Result<(), String> {
    let hwnd = guard::list_windows()
        .into_iter()
        .find(|w| w.exe.eq_ignore_ascii_case(exe) && title.is_none_or(|t| w.title.contains(t)))
        .map(|w| w.hwnd)
        .ok_or_else(|| format!("没有找到 {exe} 的窗口"))?;
    for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (verb, arg) = step.split_once(':').unwrap_or((step, ""));
        match verb {
            "size" => {
                let (w, h) = client_size(hwnd);
                println!("client {w}x{h}");
            }
            "grab" => {
                let (file, div) = arg.rsplit_once('@').map_or((arg, 1), |(f, d)| (f, d.parse().unwrap_or(1)));
                grab(hwnd, Path::new(file), div)?;
                println!("saved {file}");
            }
            // 会弹对话框的菜单项用 postcmd，不然这边会一直等到对话框关掉。
            "cmd" => send(hwnd, WM_COMMAND, arg.parse::<usize>().map_err(|_| "cmd 后面要跟菜单项编号")?, LPARAM(0)),
            "postcmd" => post(hwnd, WM_COMMAND, arg.parse::<usize>().map_err(|_| "postcmd 后面要跟菜单项编号")?, LPARAM(0)),
            // 对话框在自己的消息循环里截按键，发过去的消息不经过那里，要用投递的。
            "postkey" => {
                let k = vk(arg)?;
                post(hwnd, WM_KEYDOWN, k, LPARAM(1));
                post(hwnd, WM_KEYUP, k, LPARAM(0xC000_0001u32 as isize));
            }
            "key" => {
                let k = vk(arg)?;
                send(hwnd, WM_KEYDOWN, k, LPARAM(1));
                send(hwnd, WM_KEYUP, k, LPARAM(0xC000_0001u32 as isize));
            }
            "keydown" => send(hwnd, WM_KEYDOWN, vk(arg)?, LPARAM(1)),
            "keyup" => send(hwnd, WM_KEYUP, vk(arg)?, LPARAM(0xC000_0001u32 as isize)),
            "click" | "dblclick" => {
                let p = point(arg)?;
                send(hwnd, WM_MOUSEMOVE, 0, lparam(p));
                send(hwnd, if verb == "click" { WM_LBUTTONDOWN } else { WM_LBUTTONDBLCLK }, MK_LBUTTON, lparam(p));
                send(hwnd, WM_LBUTTONUP, 0, lparam(p));
            }
            "down" => {
                let p = point(arg)?;
                send(hwnd, WM_MOUSEMOVE, 0, lparam(p));
                send(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam(p));
            }
            "move" => send(hwnd, WM_MOUSEMOVE, MK_LBUTTON, lparam(point(arg)?)),
            "up" => send(hwnd, WM_LBUTTONUP, 0, lparam(point(arg)?)),
            "hover" => send(hwnd, WM_MOUSEMOVE, 0, lparam(point(arg)?)),
            "drag" | "timedrag" => {
                // 写法：X1,Y1>X2,Y2 或 X1,Y1>X2,Y2*步数
                let (path, steps) = arg.rsplit_once('*').map_or((arg, 20), |(p, n)| (p, n.parse().unwrap_or(20)));
                let (a, b) = path.split_once('>').ok_or("拖动要写成 X1,Y1>X2,Y2")?;
                let mut times = drag(hwnd, point(a)?, point(b)?, steps, verb == "timedrag");
                if !times.is_empty() {
                    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let mean = times.iter().sum::<f64>() / times.len() as f64;
                    println!(
                        "timedrag steps={} mean_ms={:.1} median_ms={:.1} p95_ms={:.1} max_ms={:.1}",
                        times.len(),
                        mean,
                        times[times.len() / 2],
                        times[(times.len() * 95 / 100).min(times.len() - 1)],
                        times[times.len() - 1]
                    );
                }
            }
            "wheel" => {
                // 写法：X,Y,格数。滚轮消息用的是屏幕坐标，这里的 X,Y 也是屏幕坐标。
                let v: Vec<i32> = arg.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                if v.len() != 3 {
                    return Err("wheel 要写成 X,Y,格数".into());
                }
                send(hwnd, WM_MOUSEWHEEL, ((v[2] * 120) as i16 as u16 as usize) << 16, lparam((v[0], v[1])));
            }
            "close" => post(hwnd, WM_CLOSE, 0, LPARAM(0)),
            "title" => println!("title {}", guard::title_of(hwnd)),
            "wait" => std::thread::sleep(std::time::Duration::from_millis(arg.parse().unwrap_or(100))),
            _ => return Err(format!("不认识的步骤：{step}")),
        }
    }
    Ok(())
}
