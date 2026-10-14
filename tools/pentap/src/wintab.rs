//! Wintab 接口的最小绑定。wintab32.dll 由数位板厂商驱动提供，运行时动态加载，没有就报告"不可用"。
//! 结构体和常量取自公开的 Wintab 1.4 规范。

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;

use windows::core::{s, w};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

pub const WT_PACKET: u32 = 0x7FF0;
pub const WT_PROXIMITY: u32 = 0x7FF5;

const WTI_INTERFACE: u32 = 1;
const IFC_WINTABID: u32 = 1;
const IFC_SPECVERSION: u32 = 2;
const IFC_IMPLVERSION: u32 = 3;
const IFC_NDEVICES: u32 = 4;
const IFC_NCURSORS: u32 = 6;
const WTI_CURSORS: u32 = 200;
const CSR_NAME: u32 = 1;
const WTI_DEFSYSCTX: u32 = 4;
const WTI_DEVICES: u32 = 100;
const DVC_NAME: u32 = 1;
const DVC_PKTRATE: u32 = 5;
const DVC_X: u32 = 12;
const DVC_Y: u32 = 13;
const DVC_NPRESSURE: u32 = 15;

const CXO_MESSAGES: u32 = 0x0004;

const PK_TIME: u32 = 0x0004;
const PK_SERIAL_NUMBER: u32 = 0x0010;
const PK_BUTTONS: u32 = 0x0040;
const PK_X: u32 = 0x0080;
const PK_Y: u32 = 0x0100;
const PK_NORMAL_PRESSURE: u32 = 0x0400;
const PK_ORIENTATION: u32 = 0x1000;
const PACKET_DATA: u32 = PK_TIME | PK_SERIAL_NUMBER | PK_BUTTONS | PK_X | PK_Y | PK_NORMAL_PRESSURE | PK_ORIENTATION;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Axis {
    pub min: i32,
    pub max: i32,
    pub units: u32,
    pub resolution: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LogContextW {
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
#[derive(Clone, Copy, Default, Debug)]
pub struct Packet {
    pub time: u32,
    pub serial: u32,
    pub buttons: u32,
    pub x: i32,
    pub y: i32,
    pub pressure: u32,
    pub azimuth: i32,
    pub altitude: i32,
    pub twist: i32,
}

type FnInfo = unsafe extern "system" fn(u32, u32, *mut c_void) -> u32;
type FnOpen = unsafe extern "system" fn(HWND, *mut LogContextW, i32) -> *mut c_void;
type FnClose = unsafe extern "system" fn(*mut c_void) -> i32;
type FnPacketsGet = unsafe extern "system" fn(*mut c_void, i32, *mut c_void) -> i32;
type FnQueueSizeSet = unsafe extern "system" fn(*mut c_void, i32) -> i32;

pub struct Wintab {
    info: FnInfo,
    open: FnOpen,
    close: FnClose,
    packets_get: FnPacketsGet,
    queue_size_set: FnQueueSizeSet,
}

pub struct DeviceInfo {
    pub wintab_id: String,
    pub spec_version: u32,
    pub impl_version: u32,
    pub n_devices: u32,
    pub device_name: String,
    pub pkt_rate: u32,
    pub x: Axis,
    pub y: Axis,
    pub pressure: Axis,
}

pub struct Context {
    hctx: *mut c_void,
    pub x: Axis,
    pub y: Axis,
    pub pressure_max: u32,
}

unsafe fn sym<T: Copy>(lib: HMODULE, name: windows::core::PCSTR) -> Option<T> {
    GetProcAddress(lib, name).map(|p| std::mem::transmute_copy::<_, T>(&p))
}

impl Wintab {
    pub fn load() -> Option<Self> {
        unsafe {
            let lib = LoadLibraryW(w!("wintab32.dll")).ok()?;
            Some(Self {
                info: sym(lib, s!("WTInfoW"))?,
                open: sym(lib, s!("WTOpenW"))?,
                close: sym(lib, s!("WTClose"))?,
                packets_get: sym(lib, s!("WTPacketsGet"))?,
                queue_size_set: sym(lib, s!("WTQueueSizeSet"))?,
            })
        }
    }

    unsafe fn info_u32(&self, cat: u32, idx: u32) -> u32 {
        let mut v = 0u32;
        (self.info)(cat, idx, &mut v as *mut _ as *mut c_void);
        v
    }

    unsafe fn info_str(&self, cat: u32, idx: u32) -> String {
        let mut buf = [0u16; 256];
        (self.info)(cat, idx, buf.as_mut_ptr() as *mut c_void);
        String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)])
    }

    unsafe fn info_axis(&self, cat: u32, idx: u32) -> Axis {
        let mut a = Axis::default();
        (self.info)(cat, idx, &mut a as *mut _ as *mut c_void);
        a
    }

    /// 驱动服务没在运行时，WTInfo(0,0) 返回 0。
    pub fn device_info(&self) -> Option<DeviceInfo> {
        unsafe {
            if (self.info)(0, 0, std::ptr::null_mut()) == 0 {
                return None;
            }
            Some(DeviceInfo {
                wintab_id: self.info_str(WTI_INTERFACE, IFC_WINTABID),
                spec_version: self.info_u32(WTI_INTERFACE, IFC_SPECVERSION) & 0xFFFF,
                impl_version: self.info_u32(WTI_INTERFACE, IFC_IMPLVERSION) & 0xFFFF,
                n_devices: self.info_u32(WTI_INTERFACE, IFC_NDEVICES),
                device_name: self.info_str(WTI_DEVICES, DVC_NAME),
                pkt_rate: self.info_u32(WTI_DEVICES, DVC_PKTRATE),
                x: self.info_axis(WTI_DEVICES, DVC_X),
                y: self.info_axis(WTI_DEVICES, DVC_Y),
                pressure: self.info_axis(WTI_DEVICES, DVC_NPRESSURE),
            })
        }
    }

    /// 驱动给出的默认系统上下文里的坐标映射：输入范围、输出范围、对应的屏幕范围（各是原点和大小）。
    /// 只读查询，不打开数位板。用来核对"数位板坐标怎么换成屏幕坐标"，尤其是纵轴的方向。
    pub fn default_mapping(&self) -> Option<[[i32; 2]; 6]> {
        unsafe {
            let mut lc: LogContextW = std::mem::zeroed();
            if (self.info)(WTI_DEFSYSCTX, 0, &mut lc as *mut _ as *mut c_void) == 0 {
                return None;
            }
            Some([
                [lc.in_org[0], lc.in_org[1]],
                [lc.in_ext[0], lc.in_ext[1]],
                [lc.out_org[0], lc.out_org[1]],
                [lc.out_ext[0], lc.out_ext[1]],
                lc.sys_org,
                lc.sys_ext,
            ])
        }
    }

    /// 驱动登记的各个"光标"（笔尖、橡皮头、鼠标等）的编号和名字。只读查询。
    pub fn cursors(&self) -> Vec<(u32, String)> {
        unsafe {
            let n = self.info_u32(WTI_INTERFACE, IFC_NCURSORS);
            (0..n.min(32)).map(|i| (i, self.info_str(WTI_CURSORS + i, CSR_NAME))).collect()
        }
    }

    /// 以系统上下文为模板打开（笔仍然带动系统光标），但把输出范围改成数位板原生分辨率，
    /// 这样记录到的是板子本身的坐标精度，而不是被换算成屏幕像素之后的。
    pub fn open(&self, hwnd: HWND) -> Result<Context, String> {
        unsafe {
            let dev = self.device_info().ok_or("Wintab 已加载，但没有可用的数位板（驱动服务可能没在运行）")?;
            let mut lc: LogContextW = std::mem::zeroed();
            if (self.info)(WTI_DEFSYSCTX, 0, &mut lc as *mut _ as *mut c_void) == 0 {
                return Err("取不到默认的系统上下文".into());
            }
            lc.options |= CXO_MESSAGES;
            lc.pkt_data = PACKET_DATA;
            lc.pkt_mode = 0;
            lc.move_mask = PACKET_DATA;
            lc.out_org = [0, 0, 0];
            lc.out_ext[0] = dev.x.max - dev.x.min + 1;
            lc.out_ext[1] = dev.y.max - dev.y.min + 1;
            let hctx = (self.open)(hwnd, &mut lc, 1);
            if hctx.is_null() {
                return Err("WTOpen 失败".into());
            }
            (self.queue_size_set)(hctx, 256);
            Ok(Context { hctx, x: dev.x, y: dev.y, pressure_max: dev.pressure.max.max(1) as u32 })
        }
    }

    /// 取走队列里的全部数据包，按时间先后排列。
    pub fn drain(&self, ctx: &Context, out: &mut Vec<Packet>) {
        let mut buf = [Packet::default(); 64];
        loop {
            let n = unsafe { (self.packets_get)(ctx.hctx, buf.len() as i32, buf.as_mut_ptr() as *mut c_void) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            if (n as usize) < buf.len() {
                break;
            }
        }
    }

    pub fn close(&self, ctx: Context) {
        unsafe {
            (self.close)(ctx.hctx);
        }
    }
}
