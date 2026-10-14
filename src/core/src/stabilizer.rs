// SPDX-License-Identifier: GPL-3.0-or-later
//! 稳定器：对最近若干个输入样本取等权平均（档位 N，0 即不平滑）。
//!
//! 行为要求见规格 F-016 至 F-018：
//! - 同一档位永远是同一种手感，所以用固定窗口的滑动平均，不做速度自适应；
//! - 位置窗口是 N+1 个样本，规格里各档位的抖动保留比例由它决定；
//! - 压力窗口是 N+3 个样本，落笔时填零，线头因此自然渐细。比位置窗口多出的 2 个样本
//!   是按规格 F-018 的渐细长度用仿真选定的（档位 3、每样本 5 像素时约 15 像素）；
//! - 抬笔时把窗口里还没体现出来的样本补画完，线一定画到抬笔的位置。

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub x: f32,
    pub y: f32,
    /// 压力，0 到 1
    pub p: f32,
}

pub struct Stabilizer {
    pos_window: usize,
    pressure_window: usize,
    pos: VecDeque<(f32, f32)>,
    pressure: VecDeque<f32>,
}

impl Stabilizer {
    pub fn new(level: usize) -> Self {
        Self {
            pos_window: level + 1,
            pressure_window: if level == 0 { 1 } else { level + 3 },
            pos: VecDeque::new(),
            pressure: VecDeque::new(),
        }
    }

    fn average(&self) -> Sample {
        let n = self.pos.len() as f32;
        let (x, y) = self.pos.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
        let p = self.pressure.iter().sum::<f32>() / self.pressure.len() as f32;
        Sample { x: x / n, y: y / n, p }
    }

    /// 落笔。位置窗口全部填成落笔点，线从落笔处开始；压力窗口填零。
    pub fn begin(&mut self, s: Sample) -> Sample {
        self.pos = std::iter::repeat_n((s.x, s.y), self.pos_window).collect();
        self.pressure = std::iter::repeat_n(0.0, self.pressure_window - 1).collect();
        self.pressure.push_back(s.p);
        self.average()
    }

    pub fn push(&mut self, s: Sample) -> Sample {
        self.pos.pop_front();
        self.pos.push_back((s.x, s.y));
        self.pressure.pop_front();
        self.pressure.push_back(s.p);
        self.average()
    }

    /// 抬笔。每一步丢掉最旧的样本再取平均，直到两个窗口都只剩最后一个样本，
    /// 输出依次收敛到抬笔时的位置和压力。
    pub fn finish(&mut self) -> Vec<Sample> {
        let mut out = Vec::new();
        while self.pos.len() > 1 || self.pressure.len() > 1 {
            if self.pos.len() > 1 {
                self.pos.pop_front();
            }
            if self.pressure.len() > 1 {
                self.pressure.pop_front();
            }
            out.push(self.average());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// 输入一条带正弦横向抖动的直线，返回输出抖动幅度与输入幅度之比。
    fn retention(level: usize, freq_hz: f32, rate_hz: f32) -> f32 {
        let mut st = Stabilizer::new(level);
        let amp = 3.0;
        let at = |k: usize| Sample { x: k as f32 * 5.0, y: amp * (2.0 * PI * freq_hz * k as f32 / rate_hz).sin(), p: 1.0 };
        let mut ys = Vec::new();
        st.begin(at(0));
        for k in 1..400 {
            ys.push(st.push(at(k)).y);
        }
        // 丢掉开头的过渡段，取稳态的均方根。
        let tail = &ys[100..];
        let rms = (tail.iter().map(|y| y * y).sum::<f32>() / tail.len() as f32).sqrt();
        rms / (amp / 2f32.sqrt())
    }

    #[test]
    fn level_zero_passes_samples_through() {
        let mut st = Stabilizer::new(0);
        let s = Sample { x: 3.0, y: 4.0, p: 0.7 };
        assert_eq!(st.begin(s), s);
        assert!(st.finish().is_empty());
    }

    /// 规格 F-016 的验收值：档位 3 / 8 / 15 在 4 / 10 / 20 Hz 下的保留比例，
    /// 与参照实测值相差不超过 0.05（200 Hz 输入）。
    #[test]
    fn frequency_response_matches_the_spec() {
        let reference = [
            (3, [0.982, 0.926, 0.755]),
            (8, [0.938, 0.693, 0.117]),
            (15, [0.834, 0.229, 0.189]),
        ];
        for (level, want) in reference {
            for (freq, want) in [4.0, 10.0, 20.0].into_iter().zip(want) {
                let got = retention(level, freq, 200.0);
                assert!((got - want).abs() <= 0.05, "level {level} at {freq} Hz: got {got:.3}, want {want:.3}");
            }
        }
    }

    #[test]
    fn stroke_starts_at_pen_down_with_tapered_pressure() {
        let mut st = Stabilizer::new(3);
        let first = st.begin(Sample { x: 10.0, y: 20.0, p: 1.0 });
        assert_eq!((first.x, first.y), (10.0, 20.0));
        assert!((first.p - 1.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn finish_reaches_the_last_raw_sample() {
        let mut st = Stabilizer::new(15);
        st.begin(Sample { x: 0.0, y: 0.0, p: 1.0 });
        for k in 1..50 {
            st.push(Sample { x: k as f32 * 5.0, y: 0.0, p: 1.0 });
        }
        let tail = st.finish();
        let last = tail.last().unwrap();
        assert_eq!((last.x, last.y, last.p), (245.0, 0.0, 1.0));
    }
}
