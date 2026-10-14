//! 合成笔输入。走的是 Windows 指针输入栈（WM_POINTER / Windows Ink），
//! 不经过数位板硬件和厂商的 Wintab 驱动——这是本工具的固有边界。

use windows::core::Result;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_NONE, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{PEN_MASK_PRESSURE, PT_PEN};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Hover,
    Down,
    Move,
    Up,
    Leave,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Hover => "hover",
            Phase::Down => "down",
            Phase::Move => "move",
            Phase::Up => "up",
            Phase::Leave => "leave",
        }
    }

    fn flags(self) -> POINTER_FLAGS {
        match self {
            Phase::Hover => POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE,
            Phase::Down => POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN,
            Phase::Move => POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE,
            Phase::Up => POINTER_FLAG_INRANGE | POINTER_FLAG_UP,
            Phase::Leave => POINTER_FLAG_UPDATE,
        }
    }
}

pub struct Pen {
    dev: HSYNTHETICPOINTERDEVICE,
}

impl Pen {
    pub fn new() -> Result<Self> {
        // FEEDBACK_NONE：关掉系统的笔触水波反馈，免得它被抓屏当成墨迹。
        let dev = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_NONE)? };
        Ok(Self { dev })
    }

    /// pressure: 0..=1024
    pub fn send(&self, x: i32, y: i32, pressure: u32, phase: Phase) -> Result<()> {
        let contact = matches!(phase, Phase::Down | Phase::Move);
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerFlags: phase.flags(),
                        ptPixelLocation: POINT { x, y },
                        ..Default::default()
                    },
                    penMask: PEN_MASK_PRESSURE,
                    pressure: if contact { pressure.clamp(1, 1024) } else { 0 },
                    ..Default::default()
                },
            },
        };
        unsafe { InjectSyntheticPointerInput(self.dev, &[info]) }
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        unsafe { DestroySyntheticPointerDevice(self.dev) };
    }
}
