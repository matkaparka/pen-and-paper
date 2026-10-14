// SPDX-License-Identifier: GPL-3.0-or-later
//! Wintab 笔输入（规格 F-161）。有些数位板驱动不走 Windows 的笔输入，压感只能从这里拿。
//!
//! wintab32.dll 由数位板厂商的驱动提供，运行时动态加载，没有就是"不可用"。
//! 结构体和常量取自公开的 Wintab 1.4 规范。

#![allow(clippy::upper_case_acronyms)]

use std::ffi::c_void;

use windows::core::{s, w};
use zhibi_core::lang::tr;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

pub const WT_PACKET: u32 = 0x7FF0;
pub const WT_PROXIMITY: u32 = 0x7FF5;

const WTI_DEFSYSCTX: u32 = 4;
const WTI_DEVICES: u32 = 100;
const DVC_X: u32 = 12;
const DVC_Y: u32 = 13;
const DVC_NPRESSURE: u32 = 15;
const CXO_MESSAGES: u32 = 0x0004;
const PK_STATUS: u32 = 0x0002;
const PK_CURSOR: u32 = 0x0020;
const PK_BUTTONS: u32 = 0x0040;
const PK_X: u32 = 0x0080;
const PK_Y: u32 = 0x0100;
const PK_NORMAL_PRESSURE: u32 = 0x0400;
const PACKET_DATA: u32 = PK_STATUS | PK_CURSOR | PK_BUTTONS | PK_X | PK_Y | PK_NORMAL_PRESSURE;
/// 状态位：笔倒过来了（用的是橡皮那一头）
const TPS_INVERT: u32 = 0x0010;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Axis {
    min: i32,
    max: i32,
    units: u32,
    resolution: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LogContext {
    name: [u16; 40],
    options: u32,
    status: u32,
    locks: u32,
    msg_base: u32,
    device: u32,
    pkt_rate: u32,
    pkt_data: u32,
    pkt_mode: u32,
    move_mask: u32,
    btn_dn_mask: u32,
    btn_up_mask: u32,
    in_org: [i32; 3],
    in_ext: [i32; 3],
    out_org: [i32; 3],
    out_ext: [i32; 3],
    sens: [u32; 3],
    sys_mode: i32,
    sys_org: [i32; 2],
    sys_ext: [i32; 2],
    sys_sens: [u32; 2],
}

/// 字段顺序由 PACKET_DATA 里各位从低到高决定。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Packet {
    status: u32,
    /// 驱动登记的"光标"编号。按通行的编法，除以 3 余 1 是笔尖，余 2 是橡皮
    cursor: u32,
    buttons: u32,
    x: i32,
    y: i32,
    pressure: u32,
}

type FnInfo = unsafe extern "system" fn(u32, u32, *mut c_void) -> u32;
type FnOpen = unsafe extern "system" fn(HWND, *mut LogContext, i32) -> *mut c_void;
type FnClose = unsafe extern "system" fn(*mut c_void) -> i32;
type FnPacketsGet = unsafe extern "system" fn(*mut c_void, i32, *mut c_void) -> i32;
type FnSet = unsafe extern "system" fn(*mut c_void, i32) -> i32;

/// 一个笔样本：屏幕上的位置（像素，带小数）、压力（0 到 1）、笔尖是否压着、是不是在当橡皮用
/// （笔倒过来用橡皮那头，或者在驱动里把笔切成了橡皮）。
pub type PenSample = ((f32, f32), f32, bool, bool);

/// 打开着的数位板。
pub struct Tablet {
    close: FnClose,
    packets_get: FnPacketsGet,
    enable: FnSet,
    overlap: FnSet,
    hctx: *mut c_void,
    /// 数据包里坐标的范围
    span: (f32, f32),
    /// 这个范围对应屏幕上的哪一块：左上角和大小，像素
    screen: (f32, f32, f32, f32),
    pressure_max: f32,
}

unsafe fn sym<T: Copy>(lib: HMODULE, name: windows::core::PCSTR) -> Option<T> {
    GetProcAddress(lib, name).map(|p| std::mem::transmute_copy::<_, T>(&p))
}

impl Tablet {
    /// 打开数位板，数据包的通知发给 `hwnd`。失败时返回给用户看的原因。
    pub fn open(hwnd: HWND) -> Result<Tablet, String> {
        unsafe {
            let lib = LoadLibraryW(w!("wintab32.dll")).map_err(|_| tr("这台电脑上没有装 Wintab 驱动（找不到 wintab32.dll）。", "No Wintab driver is installed on this computer (wintab32.dll not found)."))?;
            let missing = tr("wintab32.dll 里缺少需要的函数。", "wintab32.dll lacks functions this program needs.");
            let info: FnInfo = sym(lib, s!("WTInfoW")).ok_or(missing)?;
            let open: FnOpen = sym(lib, s!("WTOpenW")).ok_or(missing)?;
            let close: FnClose = sym(lib, s!("WTClose")).ok_or(missing)?;
            let packets_get: FnPacketsGet = sym(lib, s!("WTPacketsGet")).ok_or(missing)?;
            let enable: FnSet = sym(lib, s!("WTEnable")).ok_or(missing)?;
            let overlap: FnSet = sym(lib, s!("WTOverlap")).ok_or(missing)?;
            let queue: FnSet = sym(lib, s!("WTQueueSizeSet")).ok_or(missing)?;

            // 驱动的服务没在运行时，WTInfo(0, 0) 返回 0。
            if info(0, 0, std::ptr::null_mut()) == 0 {
                return Err(tr("Wintab 驱动装了，但没有找到数位板（驱动程序可能没在运行）。", "The Wintab driver is installed, but no tablet was found (the driver may not be running).").into());
            }
            let axis = |index: u32| {
                let mut a = Axis::default();
                info(WTI_DEVICES, index, &mut a as *mut _ as *mut c_void);
                a
            };
            let (ax, ay, ap) = (axis(DVC_X), axis(DVC_Y), axis(DVC_NPRESSURE));
            // 以系统上下文为模板：笔照常带动系统光标，面板上的点击仍然走鼠标消息。
            let mut lc: LogContext = std::mem::zeroed();
            if info(WTI_DEFSYSCTX, 0, &mut lc as *mut _ as *mut c_void) == 0 {
                return Err(tr("Wintab 驱动没有给出默认的设置。", "The Wintab driver did not provide its default settings.").into());
            }
            lc.options |= CXO_MESSAGES;
            lc.pkt_data = PACKET_DATA;
            lc.pkt_mode = 0;
            lc.move_mask = PACKET_DATA;
            // 输出范围改成板子的原生分辨率，这样拿到的坐标比屏幕像素细。
            let span = ((ax.max - ax.min + 1).max(1), (ay.max - ay.min + 1).max(1));
            lc.out_org = [0, 0, 0];
            lc.out_ext[0] = span.0;
            lc.out_ext[1] = span.1;
            let hctx = open(hwnd, &mut lc, 1);
            if hctx.is_null() {
                return Err(tr("打不开数位板（WTOpen 失败）。", "Could not open the tablet (WTOpen failed).").into());
            }
            queue(hctx, 256);
            Ok(Tablet {
                close,
                packets_get,
                enable,
                overlap,
                hctx,
                span: (span.0 as f32, span.1 as f32),
                screen: (lc.sys_org[0] as f32, lc.sys_org[1] as f32, lc.sys_ext[0].abs().max(1) as f32, lc.sys_ext[1].abs().max(1) as f32),
                pressure_max: ap.max.max(1) as f32,
            })
        }
    }

    /// 取走队列里的全部数据包，换算成屏幕坐标，按时间先后排列。
    pub fn drain(&mut self, out: &mut Vec<PenSample>) {
        let mut buf = [Packet::default(); 64];
        loop {
            let n = unsafe { (self.packets_get)(self.hctx, buf.len() as i32, buf.as_mut_ptr() as *mut c_void) };
            if n <= 0 {
                break;
            }
            for p in &buf[..n as usize] {
                // Wintab 规范里数位板坐标的原点在左下角，纵轴朝上；屏幕的纵轴朝下，所以倒过来。
                // （起初是拿系统光标的位置去猜方向的，用户实测颠倒了：笔刚进感应范围时光标还停在别处。）
                let (fx, fy) = (p.x as f32 / self.span.0, p.y as f32 / self.span.1);
                let (x, y) = (self.screen.0 + fx * self.screen.2, self.screen.1 + (1.0 - fy) * self.screen.3);
                let eraser = p.status & TPS_INVERT != 0 || p.cursor % 3 == 2;
                out.push(((x, y), (p.pressure as f32 / self.pressure_max).clamp(0.0, 1.0), p.buttons & 1 != 0, eraser));
            }
            if (n as usize) < buf.len() {
                break;
            }
        }
    }

    /// 窗口变成前台或退到后台时调用：别的程序也可能开着数位板，谁在前台谁收数据。
    pub fn activate(&self, active: bool) {
        unsafe {
            (self.enable)(self.hctx, active as i32);
            if active {
                (self.overlap)(self.hctx, 1);
            }
        }
    }
}

impl Drop for Tablet {
    fn drop(&mut self) {
        unsafe {
            (self.close)(self.hctx);
        }
    }
}
