//! 带时间戳的屏幕区域抓取（DXGI Desktop Duplication）。
//!
//! 每一帧的时间戳取自系统给出的 LastPresentTime：桌面合成器提交这一帧的 QPC 时刻。
//! 它不含显示器扫描输出和面板响应时间，所以由它算出的延迟是"软件侧延迟"，
//! 比人眼看到的端到端延迟短一个大致恒定的量。

use windows::core::{Interface, Result};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use windows::core::w;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, ShowWindow, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::util::Rect;

/// 桌面静止时合成器不出新帧，桌面复制就拿不到图像。
/// 在屏幕角落显示再销毁一个 2×2 的小窗口，逼合成器提交一帧。
pub fn nudge() {
    unsafe {
        let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        if let Ok(hwnd) = CreateWindowExW(ex, w!("STATIC"), w!(""), WS_POPUP, 0, 0, 2, 2, None, None, None, None) {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            std::thread::sleep(std::time::Duration::from_millis(40));
            let _ = DestroyWindow(hwnd);
        }
    }
}

pub struct OutputInfo {
    pub adapter: String,
    pub device: String,
    pub rect: Rect,
    pub rotation: i32,
}

fn utf16(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
}

fn enum_outputs() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput, OutputInfo)>> {
    let mut found = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let adesc = adapter.GetDesc1()?;
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let d = output.GetDesc()?;
                if !d.AttachedToDesktop.as_bool() {
                    continue;
                }
                let c = d.DesktopCoordinates;
                let info = OutputInfo {
                    adapter: utf16(&adesc.Description),
                    device: utf16(&d.DeviceName),
                    rect: Rect { x: c.left, y: c.top, w: c.right - c.left, h: c.bottom - c.top },
                    rotation: d.Rotation.0,
                };
                found.push((adapter.clone(), output, info));
            }
        }
    }
    Ok(found)
}

pub fn list_outputs() -> Result<Vec<OutputInfo>> {
    Ok(enum_outputs()?.into_iter().map(|(_, _, i)| i).collect())
}

pub struct FrameInfo {
    /// 合成器提交该帧的 QPC 原始计数
    pub present_ticks: i64,
    /// 自上次取帧以来累计的帧数；大于 1 说明中间有帧没被我们看到
    pub accumulated: u32,
}

pub struct Dup {
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    src_box: D3D11_BOX,
    pub w: usize,
    pub h: usize,
    /// 最近一帧的区域像素，BGRA，行紧密排列
    pub buf: Vec<u8>,
}

impl Dup {
    /// roi 用虚拟桌面坐标（物理像素）。
    pub fn new(roi: Rect) -> std::result::Result<Self, String> {
        let outputs = enum_outputs().map_err(|e| format!("枚举显示输出失败: {e}"))?;
        let (adapter, output, info) = outputs
            .into_iter()
            .find(|(_, _, i)| {
                roi.x >= i.rect.x
                    && roi.y >= i.rect.y
                    && roi.x + roi.w <= i.rect.x + i.rect.w
                    && roi.y + roi.h <= i.rect.y + i.rect.h
            })
            .ok_or_else(|| format!("区域 {roi:?} 不完全落在任何一块显示器内"))?;
        if info.rotation != DXGI_MODE_ROTATION_IDENTITY.0 && info.rotation != DXGI_MODE_ROTATION_UNSPECIFIED.0 {
            return Err("显示器处于旋转状态，本工具未处理这种情况".into());
        }
        unsafe { Self::create(&adapter, &output, roi, info.rect) }.map_err(|e| format!("创建桌面复制失败: {e}"))
    }

    unsafe fn create(adapter: &IDXGIAdapter1, output: &IDXGIOutput, roi: Rect, out_rect: Rect) -> Result<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let (device, ctx) = (device.unwrap(), ctx.unwrap());
        let dup = output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device)?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: roi.w as u32,
            Height: roi.h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;

        let (left, top) = ((roi.x - out_rect.x) as u32, (roi.y - out_rect.y) as u32);
        Ok(Self {
            ctx,
            dup,
            staging: staging.unwrap(),
            src_box: D3D11_BOX { left, top, front: 0, right: left + roi.w as u32, bottom: top + roi.h as u32, back: 1 },
            w: roi.w as usize,
            h: roi.h as usize,
            buf: vec![0u8; roi.w as usize * roi.h as usize * 4],
        })
    }

    /// 取下一帧桌面图像并把区域像素拷进 `buf`。
    /// 超时，或者这次唤醒只是鼠标指针移动（桌面图像没变），返回 Ok(None)。
    pub fn next(&mut self, timeout_ms: u32) -> Result<Option<FrameInfo>> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            if let Err(e) = self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                return if e.code() == DXGI_ERROR_WAIT_TIMEOUT { Ok(None) } else { Err(e) };
            }
            // LastPresentTime 为 0 的唤醒不带新的桌面图像（实测：复制刚建立时拿到的这种帧是全黑的）。
            let has_image = info.LastPresentTime != 0;
            if has_image {
                let tex: ID3D11Texture2D = res.as_ref().unwrap().cast()?;
                self.ctx.CopySubresourceRegion(&self.staging, 0, 0, 0, 0, &tex, 0, Some(&self.src_box));
            }
            drop(res);
            self.dup.ReleaseFrame()?;
            if !has_image {
                return Ok(None);
            }

            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
            let row = self.w * 4;
            for y in 0..self.h {
                let src = std::slice::from_raw_parts((map.pData as *const u8).add(y * map.RowPitch as usize), row);
                self.buf[y * row..(y + 1) * row].copy_from_slice(src);
            }
            self.ctx.Unmap(&self.staging, 0);
            Ok(Some(FrameInfo { present_ticks: info.LastPresentTime, accumulated: info.AccumulatedFrames }))
        }
    }
}
