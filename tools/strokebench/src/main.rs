//! strokebench：向指定屏幕区域注入确定性的合成笔画，同时带时间戳抓屏，
//! 量出"系统收到笔事件 → 画面出现墨迹"的软件侧延迟，并保存落笔前后的画面供笔迹分析。

mod bg;
mod capture;
mod cases;
mod guard;
mod inject;
mod report;
mod util;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

use capture::Dup;
use inject::{Pen, Phase};
use util::{luma, write_png, Clock, Rect};

/// 相对落笔前画面，亮度下降超过这个值的像素算"有墨"。
pub const DARK_DROP: i32 = 100;
/// 判断轨迹上某个样本点是否已出墨：看它周围 5×5 的方块里有多少有墨像素。
/// 12 个以上才算，这样程序自绘的细线光标（十字、笔刷轮廓）不会被误判成墨迹。
const INK_BOX_MIN: u32 = 12;

pub struct SampleRec {
    pub stroke: usize,
    pub phase: Phase,
    pub t_target_us: i64,
    pub t_before_us: i64,
    pub t_after_us: i64,
    pub x: i32,
    pub y: i32,
    pub pressure: u32,
    /// 在全部接触样本中的序号；非接触样本为 -1
    pub contact_idx: i64,
}

pub struct FrameRec {
    pub present_us: i64,
    pub accumulated: u32,
    pub changed_px: u32,
    pub dark_px: u32,
    /// 已出墨的最靠前的接触样本序号；还没有墨时为 -1
    pub front: i64,
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn opt_f64(args: &[String], name: &str, default: f64) -> f64 {
    opt(args, name).and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn usage() {
    println!(
        "strokebench <命令>\n\
         \n\
         \x20 list                          列出测试笔画\n\
         \x20 probe                         列出显示器和可见窗口（物理像素坐标）\n\
         \x20 shot --out F.png [--rect X,Y,W,H] [--div N]\n\
         \x20                               抓一张屏幕区域截图，--div 为缩小倍数\n\
         \x20 front --expect-exe NAME.exe    把该程序的窗口切到前台\n\
         \x20 tap --at X,Y [--to X,Y] --expect-exe NAME.exe\n\
         \x20                               用合成笔点一下或拖一段（摆测试状态用）\n\
         \x20 key --keys ctrl+z --expect-exe NAME.exe [--repeat N]\n\
         \x20                               给前台窗口发按键\n\
         \x20 run --case NAME --rect X,Y,W,H --expect-exe NAME.exe --out DIR\n\
         \x20     [--rate 200] [--speed 1000] [--pressure 0.6] [--amp 3] [--freq 10]\n\
         \x20     [--settle-ms 1000] [--countdown 0]\n\
         \x20                               注入一个测试笔画并测量\n\
         \n\
         run 会接管笔指针几秒钟。按住 Esc 可中止。\n\
         --rect 必须整个落在目标程序的空白浅色画布内，画布缩放 100%、不旋转，墨色为深色。"
    );
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("list") => {
            for (name, desc) in cases::CASES {
                println!("{name:<10} {desc}");
            }
            0
        }
        Some("probe") => cmd_probe(),
        Some("shot") => cmd_shot(&args),
        Some("run") => cmd_run(&args),
        Some("tap") => report_err(cmd_tap(&args)),
        Some("click") => report_err(cmd_click(&args)),
        Some("scroll") => report_err(cmd_scroll(&args)),
        Some("key") => report_err(cmd_key(&args)),
        Some("front") => report_err(cmd_front(&args)),
        Some("bg") => report_err(match (opt(&args, "--exe"), opt(&args, "--do")) {
            (Some(exe), Some(script)) => bg::run(&exe, opt(&args, "--title").as_deref(), &script),
            _ => Err("用法：bg --exe NAME.exe --do \"步骤;步骤\"".into()),
        }),
        _ => {
            usage();
            1
        }
    };
    std::process::exit(code);
}

fn report_err(r: Result<(), String>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            println!("中止: {e}");
            2
        }
    }
}

/// 用合成笔在一个点上点一下（或从 --at 拖到 --to），用来在测试前摆好目标程序的状态。
fn cmd_tap(args: &[String]) -> Result<(), String> {
    let pair = |name: &str| -> Option<(i32, i32)> {
        let v: Vec<i32> = opt(args, name)?.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        (v.len() == 2).then(|| (v[0], v[1]))
    };
    let (x, y) = pair("--at").ok_or("缺少 --at X,Y")?;
    let to = pair("--to");
    let exe = opt(args, "--expect-exe").ok_or("缺少 --expect-exe")?;
    let hold_ms = opt_f64(args, "--hold-ms", 60.0) as u64;
    let root = guard::window_at(x, y, &exe)?;
    guard::bring_to_front(root)?;
    guard::window_at(x, y, &exe)?;
    let held: Vec<u16> = match opt(args, "--with-keys") {
        Some(k) => parse_keys(&k)?,
        None => Vec::new(),
    };
    if !held.is_empty() {
        guard::foreground_is(&exe)?;
        send_keys(&held, false)?;
        std::thread::sleep(Duration::from_millis(80));
    }
    let result = tap_inner(x, y, to, hold_ms);
    if !held.is_empty() {
        let rev: Vec<u16> = held.iter().rev().copied().collect();
        send_keys(&rev, true)?;
    }
    result
}

fn tap_inner(x: i32, y: i32, to: Option<(i32, i32)>, hold_ms: u64) -> Result<(), String> {
    let pen = Pen::new().map_err(|e| format!("无法创建合成笔设备: {e}"))?;
    let step = |x: i32, y: i32, p: u32, ph: Phase| -> Result<(), String> {
        let r = pen.send(x, y, p, ph).map_err(|e| format!("注入失败: {e}"));
        std::thread::sleep(Duration::from_millis(8));
        r
    };
    for _ in 0..3 {
        step(x, y, 0, Phase::Hover)?;
    }
    step(x, y, 512, Phase::Down)?;
    std::thread::sleep(Duration::from_millis(hold_ms));
    let (mut ex, mut ey) = (x, y);
    if let Some((tx, ty)) = to {
        for k in 1..=40 {
            ex = x + (tx - x) * k / 40;
            ey = y + (ty - y) * k / 40;
            step(ex, ey, 512, Phase::Move)?;
        }
    }
    step(ex, ey, 0, Phase::Up)?;
    step(ex, ey, 0, Phase::Hover)?;
    step(ex, ey, 0, Phase::Leave)
}

/// 用合成鼠标在一个点上单击左键。有些界面控件不响应合成笔的点按，用它代替 tap。
fn cmd_click(args: &[String]) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
        MOUSE_EVENT_FLAGS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    let v: Vec<i32> = opt(args, "--at").ok_or("缺少 --at X,Y")?.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    if v.len() != 2 {
        return Err("--at 格式应为 X,Y".into());
    }
    let (x, y) = (v[0], v[1]);
    let exe = opt(args, "--expect-exe").ok_or("缺少 --expect-exe")?;
    let root = guard::window_at(x, y, &exe)?;
    guard::bring_to_front(root)?;
    guard::window_at(x, y, &exe)?;
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let mk = |flags: MOUSE_EVENT_FLAGS| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: ((x - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32,
                dy: ((y - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ..Default::default()
            },
        },
    };
    let right = args.iter().any(|a| a == "--right");
    let double = args.iter().any(|a| a == "--double");
    let (down, up) = if right { (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP) } else { (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP) };
    let mut seq = vec![MOUSEEVENTF_MOVE, MOUSEEVENTF_MOVE, down, up];
    if double {
        seq.extend([down, up]);
    }
    for (i, flags) in seq.into_iter().enumerate() {
        if unsafe { SendInput(&[mk(flags)], std::mem::size_of::<INPUT>() as i32) } != 1 {
            return Err("SendInput 被系统拒绝".into());
        }
        // 双击的第二下要落在系统的双击时间内，所以点击之间只留很短的间隔。
        std::thread::sleep(Duration::from_millis(if i < 2 { 60 } else { 30 }));
    }
    Ok(())
}

/// 在某个点上滚动鼠标滚轮。delta 为正表示向上（远离用户）滚，一格是 120。
fn cmd_scroll(args: &[String]) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_WHEEL, MOUSEINPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    let v: Vec<i32> = opt(args, "--at").ok_or("缺少 --at X,Y")?.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    if v.len() != 2 {
        return Err("--at 格式应为 X,Y".into());
    }
    let (x, y) = (v[0], v[1]);
    let exe = opt(args, "--expect-exe").ok_or("缺少 --expect-exe")?;
    let delta = opt_f64(args, "--delta", 120.0) as i32;
    let root = guard::window_at(x, y, &exe)?;
    guard::bring_to_front(root)?;
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let mv = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: ((x - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32,
                dy: ((y - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ..Default::default()
            },
        },
    };
    let wheel = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { mouseData: delta as u32, dwFlags: MOUSEEVENTF_WHEEL, ..Default::default() } },
    };
    for inp in [mv, mv, wheel] {
        if unsafe { SendInput(&[inp], std::mem::size_of::<INPUT>() as i32) } != 1 {
            return Err("SendInput 被系统拒绝".into());
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    Ok(())
}

/// 把属于某个程序的窗口切到前台（不点击它）。
fn cmd_front(args: &[String]) -> Result<(), String> {
    let exe = opt(args, "--expect-exe").ok_or("缺少 --expect-exe")?;
    let hwnd = guard::list_windows()
        .into_iter()
        .find(|w| w.exe.eq_ignore_ascii_case(&exe))
        .map(|w| w.hwnd)
        .ok_or_else(|| format!("没有找到 {exe} 的可见窗口"))?;
    guard::bring_to_front(hwnd)
}

fn parse_keys(combo: &str) -> Result<Vec<u16>, String> {
    let vk_of = |name: &str| -> Option<u16> {
        Some(match name {
            "ctrl" => 0x11,
            "shift" => 0x10,
            "alt" => 0x12,
            "delete" => 0x2E,
            "esc" => 0x1B,
            "enter" => 0x0D,
            "tab" => 0x09,
            "space" => 0x20,
            "home" => 0x24,
            "pageup" => 0x21,
            "pagedown" => 0x22,
            "backspace" => 0x08,
            "insert" => 0x2D,
            "end" => 0x23,
            "left" => 0x25,
            "up" => 0x26,
            "right" => 0x27,
            "down" => 0x28,
            "[" => 0xDB,
            "]" => 0xDD,
            "numplus" => 0x6B,
            "numminus" => 0x6D,
            s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphanumeric() => s.as_bytes()[0].to_ascii_uppercase() as u16,
            s if s.starts_with('f') => 0x6F + s[1..].parse::<u16>().ok().filter(|n| (1..=12).contains(n))?,
            _ => return None,
        })
    };
    combo
        .to_ascii_lowercase()
        .split('+')
        .map(|k| vk_of(k.trim()).ok_or_else(|| format!("不认识的按键 {k}")))
        .collect()
}

/// 按下（up = false）或松开（up = true）一组键，按给定顺序。
fn send_keys(keys: &[u16], up: bool) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };
    let seq: Vec<INPUT> = keys
        .iter()
        .map(|&vk| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() },
            },
        })
        .collect();
    if unsafe { SendInput(&seq, std::mem::size_of::<INPUT>() as i32) } as usize != seq.len() {
        return Err("SendInput 被系统拒绝".into());
    }
    Ok(())
}

/// 给前台窗口发一个按键组合，例如 ctrl+z、delete、ctrl+shift+n。
fn cmd_key(args: &[String]) -> Result<(), String> {
    let combo = opt(args, "--keys").ok_or("缺少 --keys")?;
    let exe = opt(args, "--expect-exe").ok_or("缺少 --expect-exe")?;
    let repeat = opt_f64(args, "--repeat", 1.0) as usize;
    let keys = parse_keys(&combo)?;
    let rev: Vec<u16> = keys.iter().rev().copied().collect();
    for _ in 0..repeat {
        guard::foreground_is(&exe)?;
        send_keys(&keys, false)?;
        send_keys(&rev, true)?;
        std::thread::sleep(Duration::from_millis(120));
    }
    Ok(())
}

fn cmd_probe() -> i32 {
    match capture::list_outputs() {
        Ok(outs) => {
            for o in outs {
                println!("output {} on \"{}\": {:?} rotation={}", o.device, o.adapter, o.rect, o.rotation);
            }
        }
        Err(e) => println!("枚举显示输出失败: {e}"),
    }
    for w in guard::list_windows() {
        let r = w.rect;
        println!(
            "{} {:<22} {},{},{},{}  \"{}\"",
            if w.foreground { "*" } else { " " },
            w.exe,
            r.x,
            r.y,
            r.w,
            r.h,
            w.title
        );
    }
    0
}

fn grab(roi: Rect) -> Result<Dup, String> {
    let mut dup = Dup::new(roi)?;
    for _ in 0..20 {
        capture::nudge();
        match dup.next(200) {
            Ok(Some(_)) => return Ok(dup),
            Ok(None) => {}
            Err(e) => return Err(format!("取帧失败: {e}")),
        }
    }
    Err("等不到第一帧桌面图像".into())
}

fn cmd_shot(args: &[String]) -> i32 {
    let Some(out) = opt(args, "--out") else {
        usage();
        return 1;
    };
    let roi = match opt(args, "--rect") {
        Some(s) => match Rect::parse(&s) {
            Some(r) => r,
            None => {
                println!("--rect 格式应为 X,Y,W,H");
                return 1;
            }
        },
        None => match capture::list_outputs() {
            Ok(o) if !o.is_empty() => o[0].rect,
            _ => {
                println!("找不到显示器");
                return 1;
            }
        },
    };
    let div = opt_f64(args, "--div", 1.0) as usize;
    match grab(roi) {
        Ok(dup) => match write_png(Path::new(&out), &dup.buf, dup.w, dup.h, div) {
            Ok(()) => {
                println!("saved {out} region={roi:?} div={div}");
                0
            }
            Err(e) => {
                println!("写 PNG 失败: {e}");
                1
            }
        },
        Err(e) => {
            println!("{e}");
            1
        }
    }
}

/// 整个区域相对参考画面的变化统计：(有任何变化的像素数, 有墨像素数)。
fn scan(buf: &[u8], refb: &[u8]) -> (u32, u32) {
    let (mut changed, mut dark) = (0u32, 0u32);
    for (a, b) in buf.as_chunks::<4>().0.iter().zip(refb.as_chunks::<4>().0) {
        if a[..3] != b[..3] {
            let d = luma(b[0], b[1], b[2]) - luma(a[0], a[1], a[2]);
            if d.abs() > 8 {
                changed += 1;
            }
            if d > DARK_DROP {
                dark += 1;
            }
        }
    }
    (changed, dark)
}

fn inked_at(buf: &[u8], refb: &[u8], w: usize, h: usize, x: i32, y: i32) -> bool {
    let mut n = 0u32;
    for yy in (y - 2).max(0)..=(y + 2).min(h as i32 - 1) {
        for xx in (x - 2).max(0)..=(x + 2).min(w as i32 - 1) {
            let i = (yy as usize * w + xx as usize) * 4;
            if luma(refb[i], refb[i + 1], refb[i + 2]) - luma(buf[i], buf[i + 1], buf[i + 2]) > DARK_DROP {
                n += 1;
            }
        }
    }
    n >= INK_BOX_MIN
}

fn cmd_run(args: &[String]) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(e) => {
            println!("中止: {e}");
            2
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let case = opt(args, "--case").ok_or("缺少 --case")?;
    let roi = opt(args, "--rect").and_then(|s| Rect::parse(&s)).ok_or("缺少或无法解析 --rect X,Y,W,H")?;
    let expect_exe = opt(args, "--expect-exe").ok_or("缺少 --expect-exe（用来确认笔画落在正确的程序里）")?;
    let out_dir = PathBuf::from(opt(args, "--out").ok_or("缺少 --out 目录")?);
    let params = cases::Params {
        w: roi.w as f64,
        h: roi.h as f64,
        rate_hz: opt_f64(args, "--rate", 200.0),
        speed: opt_f64(args, "--speed", 1000.0),
        pressure: opt_f64(args, "--pressure", 0.6),
        amp: opt_f64(args, "--amp", 3.0),
        freq: opt_f64(args, "--freq", 10.0),
    };
    let settle_ms = opt_f64(args, "--settle-ms", 1000.0) as u64;
    let countdown = opt_f64(args, "--countdown", 0.0) as u64;
    let strokes = cases::build(&case, &params).ok_or_else(|| format!("未知用例 {case}，用 list 查看"))?;

    for s in (1..=countdown).rev() {
        println!("{s}...");
        std::thread::sleep(Duration::from_secs(1));
    }

    let root = guard::target_window(roi, &expect_exe)?;
    guard::bring_to_front(root)?;
    let title = guard::title_of(root);

    let clock = Clock::new();
    let mut dup = grab(roi)?;
    // 再等 250 ms，把切前台引起的重绘收完，取稳定后的画面作参考。
    let until = clock.now_us() + 250_000;
    while clock.now_us() < until {
        dup.next(30).map_err(|e| format!("取帧失败: {e}"))?;
    }
    let refbuf = dup.buf.clone();
    let (w, h) = (dup.w, dup.h);

    // 全部接触样本的区域内坐标，供抓屏线程判断墨迹前沿。
    let path: Vec<(i32, i32)> = strokes
        .iter()
        .flat_map(|s| s.pts.iter().map(|p| (p.x.round() as i32, p.y.round() as i32)))
        .collect();
    let bright = path.iter().filter(|&&(x, y)| {
        let i = (y.clamp(0, h as i32 - 1) as usize * w + x.clamp(0, w as i32 - 1) as usize) * 4;
        luma(refbuf[i], refbuf[i + 1], refbuf[i + 2]) >= 200
    });
    if bright.count() < path.len() {
        return Err("笔画路径上的参考画面不是浅色空白画布；请清空画布或调整 --rect".into());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let injected = Arc::new(AtomicI64::new(-1));
    let cap = {
        let (stop, injected, refbuf, path) = (stop.clone(), injected.clone(), refbuf.clone(), path.clone());
        std::thread::spawn(move || {
            let mut frames: Vec<FrameRec> = Vec::new();
            let mut front = -1i64;
            let mut err = None;
            while !stop.load(Ordering::Relaxed) {
                match dup.next(20) {
                    Ok(Some(fi)) => {
                        let inj = injected.load(Ordering::Relaxed);
                        // 墨迹不可能跑到笔的前面，所以只在已注入的样本范围内找前沿。
                        for i in (front + 1)..=inj {
                            let (x, y) = path[i as usize];
                            if inked_at(&dup.buf, &refbuf, w, h, x, y) {
                                front = i;
                            }
                        }
                        let (changed_px, dark_px) = scan(&dup.buf, &refbuf);
                        frames.push(FrameRec {
                            present_us: clock.ticks_to_us(fi.present_ticks),
                            accumulated: fi.accumulated,
                            changed_px,
                            dark_px,
                            front,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        err = Some(format!("取帧失败: {e}"));
                        break;
                    }
                }
            }
            (frames, dup.buf, err)
        })
    };

    let inject_result = inject_strokes(&strokes, roi, &expect_exe, root, &clock, &injected, params.rate_hz);
    std::thread::sleep(Duration::from_millis(settle_ms));
    stop.store(true, Ordering::Relaxed);
    let (frames, after, cap_err) = cap.join().map_err(|_| "抓屏线程崩溃")?;
    let recs = inject_result?;
    if let Some(e) = cap_err {
        return Err(e);
    }

    fs::create_dir_all(&out_dir).map_err(|e| format!("无法创建输出目录: {e}"))?;
    write_png(&out_dir.join("before.png"), &refbuf, w, h, 1).map_err(|e| e.to_string())?;
    write_png(&out_dir.join("after.png"), &after, w, h, 1).map_err(|e| e.to_string())?;
    let run = report::Run { case: &case, params: &params, roi, exe: &expect_exe, title: &title, settle_ms, w, h };
    report::write_all(&out_dir, &run, &recs, &frames, &refbuf, &after).map_err(|e| e.to_string())
}

fn inject_strokes(
    strokes: &[cases::Stroke],
    roi: Rect,
    expect_exe: &str,
    root: windows::Win32::Foundation::HWND,
    clock: &Clock,
    injected: &AtomicI64,
    rate_hz: f64,
) -> Result<Vec<SampleRec>, String> {
    let pen = Pen::new().map_err(|e| format!("无法创建合成笔设备: {e}"))?;
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    let dt_us = (1e6 / rate_hz).round() as i64;
    let mut recs: Vec<SampleRec> = Vec::new();
    let mut contact_idx = -1i64;

    for (si, stroke) in strokes.iter().enumerate() {
        if guard::target_window(roi, expect_exe)? != root || !guard::is_foreground(root) {
            return Err("目标窗口在测试过程中被遮挡或失去前台".into());
        }
        let to_screen = |p: &cases::Pt| (roi.x + p.x.round() as i32, roi.y + p.y.round() as i32);
        let (first, last) = (stroke.pts[0], *stroke.pts.last().unwrap());
        // 落笔前先悬停 150 ms：真实的笔也是先进入感应范围再接触；
        // 更要紧的是让程序自绘的笔刷光标在落笔前就出现，这样它会被算进"落笔前的底噪"，
        // 不会被误当成第一笔墨迹。
        let n_hover = ((0.15 * rate_hz).round() as i64).max(3);
        let base = clock.now_us() + (n_hover + 1) * dt_us + 20_000;

        let mut send = |phase: Phase, pt: &cases::Pt, t_target: i64, recs: &mut Vec<SampleRec>| -> Result<(), String> {
            clock.wait_until_us(t_target);
            let (x, y) = to_screen(pt);
            let pressure = (pt.p * 1024.0).round() as u32;
            let contact = matches!(phase, Phase::Down | Phase::Move);
            let t_before = clock.now_us();
            let r = pen.send(x, y, pressure, phase);
            let t_after = clock.now_us();
            if contact && r.is_ok() {
                contact_idx += 1;
                injected.store(contact_idx, Ordering::Relaxed);
            }
            recs.push(SampleRec {
                stroke: si,
                phase,
                t_target_us: t_target,
                t_before_us: t_before,
                t_after_us: t_after,
                x,
                y,
                pressure: if contact { pressure.clamp(1, 1024) } else { 0 },
                contact_idx: if contact && r.is_ok() { contact_idx } else { -1 },
            });
            r.map_err(|e| format!("注入失败（{}）: {e}", phase.name()))
        };

        for k in (1..=n_hover).rev() {
            send(Phase::Hover, &first, base - k * dt_us, &mut recs)?;
        }
        let mut aborted = false;
        let mut end = last;
        for (k, pt) in stroke.pts.iter().enumerate() {
            if guard::esc_pressed() {
                aborted = true;
                end = *pt;
                break;
            }
            let phase = if k == 0 { Phase::Down } else { Phase::Move };
            if let Err(e) = send(phase, pt, base + pt.t_us, &mut recs) {
                let _ = pen.send(to_screen(pt).0, to_screen(pt).1, 0, Phase::Up);
                let _ = pen.send(to_screen(pt).0, to_screen(pt).1, 0, Phase::Leave);
                return Err(e);
            }
            end = *pt;
        }
        let t_end = clock.now_us().max(base + end.t_us);
        // 抬笔后把悬停位置移到区域外再离开，免得程序自绘的笔刷光标留在画面里干扰收笔处的分析。
        let park = cases::Pt { t_us: 0, x: -30.0, y: -30.0, p: 0.0 };
        send(Phase::Up, &end, t_end + dt_us, &mut recs)?;
        send(Phase::Hover, &park, t_end + 2 * dt_us, &mut recs)?;
        send(Phase::Leave, &park, t_end + 3 * dt_us, &mut recs)?;
        if aborted {
            return Err("用户按 Esc 中止".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(recs)
}
