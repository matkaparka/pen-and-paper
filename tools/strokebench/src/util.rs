use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// 以 QPC 为基准的时钟。所有时间戳都换算成"相对 t0 的微秒"，
/// 这样注入时间和 DXGI 给出的呈现时间落在同一条时间轴上。
#[derive(Clone, Copy)]
pub struct Clock {
    qpf: i64,
    t0: i64,
}

impl Clock {
    pub fn new() -> Self {
        let (mut qpf, mut t0) = (0i64, 0i64);
        unsafe {
            let _ = QueryPerformanceFrequency(&mut qpf);
            let _ = QueryPerformanceCounter(&mut t0);
        }
        Self { qpf, t0 }
    }

    pub fn ticks_to_us(&self, ticks: i64) -> i64 {
        ((ticks - self.t0) as i128 * 1_000_000 / self.qpf as i128) as i64
    }

    pub fn now_us(&self) -> i64 {
        let mut v = 0i64;
        unsafe {
            let _ = QueryPerformanceCounter(&mut v);
        }
        self.ticks_to_us(v)
    }

    /// 等到指定时刻。远的时候睡眠，最后几毫秒自旋，保证注入时刻的精度。
    pub fn wait_until_us(&self, target_us: i64) {
        loop {
            let remain = target_us - self.now_us();
            if remain <= 0 {
                return;
            }
            if remain > 4000 {
                std::thread::sleep(std::time::Duration::from_micros((remain - 3000) as u64));
            } else {
                std::hint::spin_loop();
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn parse(s: &str) -> Option<Self> {
        let v: Vec<i32> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        (v.len() == 4 && v[2] > 0 && v[3] > 0).then(|| Rect { x: v[0], y: v[1], w: v[2], h: v[3] })
    }
}

#[inline]
pub fn luma(b: u8, g: u8, r: u8) -> i32 {
    (r as i32 * 299 + g as i32 * 587 + b as i32 * 114) / 1000
}

/// 把 BGRA 缓冲写成 PNG；div > 1 时先做 div×div 的均值缩小。
pub fn write_png(path: &Path, bgra: &[u8], w: usize, h: usize, div: usize) -> std::io::Result<()> {
    let div = div.max(1);
    let (ow, oh) = (w / div, h / div);
    let mut rgb = vec![0u8; ow * oh * 3];
    for oy in 0..oh {
        for ox in 0..ow {
            let mut acc = [0u32; 3];
            for dy in 0..div {
                for dx in 0..div {
                    let i = ((oy * div + dy) * w + ox * div + dx) * 4;
                    acc[0] += bgra[i + 2] as u32;
                    acc[1] += bgra[i + 1] as u32;
                    acc[2] += bgra[i] as u32;
                }
            }
            let n = (div * div) as u32;
            let o = (oy * ow + ox) * 3;
            rgb[o] = (acc[0] / n) as u8;
            rgb[o + 1] = (acc[1] / n) as u8;
            rgb[o + 2] = (acc[2] / n) as u8;
        }
    }
    let mut enc = png::Encoder::new(BufWriter::new(File::create(path)?), ow as u32, oh as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(std::io::Error::other)?;
    wr.write_image_data(&rgb).map_err(std::io::Error::other)
}

pub fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let pos = q * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}
