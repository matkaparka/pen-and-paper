//! pentap：笔输入记录器。
//!
//! 开一个白底窗口，把收到的每个笔输入样本原样写进 CSV，同时用 GDI 立即画线。
//! 输入接口二选一：Windows 指针输入（WM_POINTER，含历史样本）或 Wintab 数据包。两个用途：
//!   1. 记录真实数位板的原始输入特性（报告率、压感分辨率、抖动）。
//!   2. 作为 strokebench 的自检靶子和"最简实现"的延迟下限参照。

#![allow(non_snake_case)]

mod wintab;

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufWriter, Write};

use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreatePen, DeleteObject, GetDC, GetStockObject, InvalidateRect, LineTo, MoveToEx, ReleaseDC,
    ScreenToClient, SelectObject, HBRUSH, PS_SOLID, WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_C, VK_ESCAPE};
use windows::Win32::UI::Input::Pointer::{GetPointerPenInfoHistory, GetPointerType, POINTER_PEN_INFO};
use windows::Win32::UI::WindowsAndMessaging::*;

struct App {
    out: Option<BufWriter<File>>,
    qpf: i64,
    t0: i64,
    max_width: f64,
    last: Option<(i32, i32)>,
    n_samples: u64,
    first_us: i64,
    last_us: i64,
    p_min: u32,
    p_max: u32,
    wt: Option<(wintab::Wintab, wintab::Context)>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe { QueryPerformanceCounter(&mut v).ok() };
    v
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn parse_pair(s: &str) -> Option<(i32, i32)> {
    let (a, b) = s.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "pentap [--api pointer|wintab] [--out FILE.csv] [--pos X,Y] [--size W,H] [--max-width PX] [--topmost] [--duration SEC]\n\
             pentap --wintab-info      只打印 Wintab 驱动和数位板的信息\n\
             Esc 退出，C 清屏。坐标与尺寸均为物理像素（客户区）。"
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--wintab-info") {
        print_wintab_info();
        return Ok(());
    }
    let use_wintab = match arg_value(&args, "--api").as_deref() {
        None | Some("pointer") => false,
        Some("wintab") => true,
        Some(other) => {
            println!("未知的 --api {other}，可选 pointer 或 wintab");
            return Ok(());
        }
    };
    let pos = arg_value(&args, "--pos").and_then(|s| parse_pair(&s)).unwrap_or((200, 200));
    let size = arg_value(&args, "--size").and_then(|s| parse_pair(&s)).unwrap_or((1400, 700));
    let max_width: f64 = arg_value(&args, "--max-width").and_then(|s| s.parse().ok()).unwrap_or(12.0);
    let duration: Option<f64> = arg_value(&args, "--duration").and_then(|s| s.parse().ok());
    let topmost = args.iter().any(|a| a == "--topmost");

    let out = match arg_value(&args, "--out") {
        Some(path) => {
            let mut w = BufWriter::new(File::create(&path).expect("无法创建输出文件"));
            if !use_wintab {
                writeln!(
                    w,
                    "recv_us,msg,pointer_id,pointer_type,sample_us,dw_time,hist_index,hist_count,\
                     x_px,y_px,x_himetric,y_himetric,x_raw_px,y_raw_px,x_raw_himetric,y_raw_himetric,\
                     pressure,tilt_x,tilt_y,rotation,pen_flags,pen_mask,pointer_flags"
                )
                .unwrap();
            }
            Some(w)
        }
        None => None,
    };

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mut qpf = 0i64;
        QueryPerformanceFrequency(&mut qpf)?;
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                out,
                qpf,
                t0: qpc(),
                max_width,
                last: None,
                n_samples: 0,
                first_us: 0,
                last_us: 0,
                p_min: u32::MAX,
                p_max: 0,
                wt: None,
            })
        });

        let hinst = GetModuleHandleW(None)?;
        let class = w!("pentap_window");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_CROSS)?,
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let style = WS_OVERLAPPEDWINDOW;
        let ex = if topmost { WS_EX_TOPMOST } else { WINDOW_EX_STYLE(0) };
        // --pos / --size 指客户区，换算出外框。
        let mut rc = RECT { left: pos.0, top: pos.1, right: pos.0 + size.0, bottom: pos.1 + size.1 };
        AdjustWindowRectEx(&mut rc, style, false, ex)?;
        let hwnd = CreateWindowExW(
            ex,
            class,
            w!("pentap - pen input recorder (Esc = quit, C = clear)"),
            style | WS_VISIBLE,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        let _ = SetForegroundWindow(hwnd);
        if use_wintab {
            if let Err(e) = open_wintab(hwnd) {
                println!("Wintab 不可用: {e}");
                let _ = DestroyWindow(hwnd);
                return Ok(());
            }
        }
        if let Some(sec) = duration {
            SetTimer(Some(hwnd), 1, (sec * 1000.0) as u32, None);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            if let Some(w) = app.out.as_mut() {
                let _ = w.flush();
            }
            if let Some((wt, ctx)) = app.wt.take() {
                wt.close(ctx);
            }
            let span_s = (app.last_us - app.first_us) as f64 / 1e6;
            println!("samples={}", app.n_samples);
            if app.n_samples > 1 && span_s > 0.0 {
                println!("span_s={:.3} mean_rate_hz={:.1}", span_s, (app.n_samples - 1) as f64 / span_s);
                println!("pressure_min={} pressure_max={}", app.p_min, app.p_max);
            }
        }
    });
    Ok(())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP if !wintab_active() => {
                on_pointer(hwnd, msg, wp);
                LRESULT(0)
            }
            wintab::WT_PACKET => {
                on_wintab(hwnd);
                LRESULT(0)
            }
            wintab::WT_PROXIMITY => {
                APP.with(|a| {
                    if let Some(app) = a.borrow_mut().as_mut() {
                        app.last = None;
                    }
                });
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wp.0 == VK_ESCAPE.0 as usize {
                    let _ = DestroyWindow(hwnd);
                } else if wp.0 == VK_C.0 as usize {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                let _ = DestroyWindow(hwnd);
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

unsafe fn on_pointer(hwnd: HWND, msg: u32, wp: WPARAM) {
    let recv = qpc();
    let pointer_id = (wp.0 & 0xFFFF) as u32;
    let mut ptype = POINTER_INPUT_TYPE(0);
    let _ = GetPointerType(pointer_id, &mut ptype);

    // 一条消息可能带多个合并样本；历史数组按"最新在前"返回。
    let mut count = 64u32;
    let mut hist = [POINTER_PEN_INFO::default(); 64];
    if GetPointerPenInfoHistory(pointer_id, &mut count, Some(hist.as_mut_ptr())).is_err() {
        return;
    }
    let n = (count as usize).min(hist.len());
    let name = match msg {
        WM_POINTERDOWN => "down",
        WM_POINTERUP => "up",
        _ => "update",
    };

    APP.with(|a| {
        let mut guard = a.borrow_mut();
        let Some(app) = guard.as_mut() else { return };
        let to_us = |ticks: i64| (ticks - app.t0) as i128 * 1_000_000 / app.qpf as i128;
        let recv_us = to_us(recv) as i64;
        let hdc = GetDC(Some(hwnd));

        for (k, info) in hist[..n].iter().enumerate().rev() {
            let pi = &info.pointerInfo;
            let sample_us = if pi.PerformanceCount != 0 { to_us(pi.PerformanceCount as i64) as i64 } else { recv_us };
            if let Some(w) = app.out.as_mut() {
                let _ = writeln!(
                    w,
                    "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    recv_us, name, pointer_id, ptype.0, sample_us, pi.dwTime, k, count,
                    pi.ptPixelLocation.x, pi.ptPixelLocation.y,
                    pi.ptHimetricLocation.x, pi.ptHimetricLocation.y,
                    pi.ptPixelLocationRaw.x, pi.ptPixelLocationRaw.y,
                    pi.ptHimetricLocationRaw.x, pi.ptHimetricLocationRaw.y,
                    info.pressure, info.tiltX, info.tiltY, info.rotation,
                    info.penFlags, info.penMask, pi.pointerFlags.0
                );
            }
            if app.n_samples == 0 {
                app.first_us = sample_us;
            }
            app.last_us = sample_us;
            app.n_samples += 1;

            let in_contact = pi.pointerFlags.0 & 0x4 != 0;
            if in_contact {
                app.p_min = app.p_min.min(info.pressure);
                app.p_max = app.p_max.max(info.pressure);
                let mut pt = POINT { x: pi.ptPixelLocation.x, y: pi.ptPixelLocation.y };
                let _ = ScreenToClient(hwnd, &mut pt);
                let width = (1.0 + app.max_width * info.pressure as f64 / 1024.0).round() as i32;
                let pen = CreatePen(PS_SOLID, width, COLORREF(0));
                let old = SelectObject(hdc, pen.into());
                let (x0, y0) = app.last.unwrap_or((pt.x, pt.y));
                let _ = MoveToEx(hdc, x0, y0, None);
                let _ = LineTo(hdc, pt.x, pt.y);
                // LineTo 不画终点像素；补一个点，保证单点落笔也可见。
                let _ = LineTo(hdc, pt.x + 1, pt.y);
                SelectObject(hdc, old);
                let _ = DeleteObject(pen.into());
                app.last = Some((pt.x, pt.y));
            } else {
                app.last = None;
            }
        }
        if msg == WM_POINTERUP {
            app.last = None;
        }
        ReleaseDC(Some(hwnd), hdc);
    });
}

fn wintab_active() -> bool {
    APP.with(|a| a.borrow().as_ref().is_some_and(|app| app.wt.is_some()))
}

fn print_wintab_info() {
    let Some(wt) = wintab::Wintab::load() else {
        println!("wintab32.dll 无法加载（没有安装提供 Wintab 的数位板驱动）");
        return;
    };
    match wt.device_info() {
        None => println!("wintab32.dll 已加载，但 WTInfo 返回 0：没有可用的数位板，驱动服务可能没在运行"),
        Some(d) => {
            println!("wintab_id      = {}", d.wintab_id);
            println!("spec_version   = {}.{}", d.spec_version >> 8, d.spec_version & 0xFF);
            println!("impl_version   = {}.{}", d.impl_version >> 8, d.impl_version & 0xFF);
            println!("devices        = {}", d.n_devices);
            println!("device_name    = {}", d.device_name);
            println!("pkt_rate_hz    = {}", d.pkt_rate);
            println!("x_axis         = {}..{} units={} resolution={}", d.x.min, d.x.max, d.x.units, d.x.resolution >> 16);
            println!("y_axis         = {}..{} units={} resolution={}", d.y.min, d.y.max, d.y.units, d.y.resolution >> 16);
            println!("pressure_axis  = {}..{}", d.pressure.min, d.pressure.max);
            if let Some([in_org, in_ext, out_org, out_ext, sys_org, sys_ext]) = wt.default_mapping() {
                println!("default context: in  org={in_org:?} ext={in_ext:?}");
                println!("default context: out org={out_org:?} ext={out_ext:?}");
                println!("default context: sys org={sys_org:?} ext={sys_ext:?}");
            }
            for (i, name) in wt.cursors().into_iter().filter(|c| !c.1.trim().is_empty()) {
                println!("cursor {i:<2}      = {name}");
            }
        }
    }
}

fn open_wintab(hwnd: HWND) -> std::result::Result<(), String> {
    let wt = wintab::Wintab::load().ok_or("wintab32.dll 无法加载")?;
    let ctx = wt.open(hwnd)?;
    APP.with(|a| {
        let mut guard = a.borrow_mut();
        let app = guard.as_mut().unwrap();
        if let Some(w) = app.out.as_mut() {
            // 第一行是元数据注释，读取时跳过以 # 开头的行。
            let _ = writeln!(
                w,
                "# wintab x={}..{} y={}..{} pressure_max={}",
                ctx.x.min, ctx.x.max, ctx.y.min, ctx.y.max, ctx.pressure_max
            );
            let _ = writeln!(w, "recv_us,pkt_time_ms,serial,buttons,x,y,pressure,azimuth,altitude,twist");
        }
        app.wt = Some((wt, ctx));
    });
    Ok(())
}

unsafe fn on_wintab(hwnd: HWND) {
    let recv = qpc();
    APP.with(|a| {
        let mut guard = a.borrow_mut();
        let Some(app) = guard.as_mut() else { return };
        let Some((wt, ctx)) = app.wt.as_ref() else { return };
        let mut pkts = Vec::new();
        wt.drain(ctx, &mut pkts);
        let pressure_max = ctx.pressure_max;
        let recv_us = ((recv - app.t0) as i128 * 1_000_000 / app.qpf as i128) as i64;

        for p in &pkts {
            if let Some(w) = app.out.as_mut() {
                let _ = writeln!(
                    w,
                    "{},{},{},{},{},{},{},{},{},{}",
                    recv_us, p.time, p.serial, p.buttons, p.x, p.y, p.pressure, p.azimuth, p.altitude, p.twist
                );
            }
            let t_us = p.time as i64 * 1000;
            if app.n_samples == 0 {
                app.first_us = t_us;
            }
            app.last_us = t_us;
            app.n_samples += 1;
            if p.pressure > 0 {
                app.p_min = app.p_min.min(p.pressure);
                app.p_max = app.p_max.max(p.pressure);
            }
        }

        // 画面反馈用系统光标的位置（系统上下文下笔会带动光标），精度只到像素；
        // 数位板原生精度的坐标在 CSV 里。
        let Some(last) = pkts.last() else { return };
        if last.pressure == 0 {
            app.last = None;
            return;
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = ScreenToClient(hwnd, &mut pt);
        let hdc = GetDC(Some(hwnd));
        let width = (1.0 + app.max_width * last.pressure as f64 / pressure_max as f64).round() as i32;
        let pen = CreatePen(PS_SOLID, width, COLORREF(0));
        let old = SelectObject(hdc, pen.into());
        let (x0, y0) = app.last.unwrap_or((pt.x, pt.y));
        let _ = MoveToEx(hdc, x0, y0, None);
        let _ = LineTo(hdc, pt.x, pt.y);
        let _ = LineTo(hdc, pt.x + 1, pt.y);
        SelectObject(hdc, old);
        let _ = DeleteObject(pen.into());
        ReleaseDC(Some(hwnd), hdc);
        app.last = Some((pt.x, pt.y));
    });
}
