//! 固定测试笔画。坐标是相对测试区域左上角的像素，压力 0..1，时间是相对落笔的微秒。
//! 每个用例都是确定性的：同样的参数永远生成同样的样本序列，三款软件吃到的输入完全一致。

use std::f64::consts::PI;

#[derive(Clone, Copy, Debug)]
pub struct Pt {
    pub t_us: i64,
    pub x: f64,
    pub y: f64,
    pub p: f64,
}

pub struct Stroke {
    pub pts: Vec<Pt>,
}

pub struct Params {
    pub w: f64,
    pub h: f64,
    pub rate_hz: f64,
    /// 笔尖沿主方向的速度，像素/秒
    pub speed: f64,
    /// 恒压用例的压力，以及变压用例的峰值
    pub pressure: f64,
    /// jitter 用例的横向抖动幅度（像素）与频率（Hz）
    pub amp: f64,
    pub freq: f64,
}

pub const MARGIN: f64 = 40.0;

pub const CASES: &[(&str, &str)] = &[
    ("line", "水平直线，恒速恒压"),
    ("ramp", "水平直线，压力 轻→满→轻（三角形），用于压感-粗细曲线"),
    ("ramp-up", "水平直线，压力从轻线性升到满（轻压起笔、重压收笔）"),
    ("ramp-down", "水平直线，压力从满线性降到轻（重压起笔、轻压收笔）"),
    ("flick", "水平快速笔画，压力快起慢收，模拟自然的入锋出锋"),
    ("jitter", "水平直线叠加已知幅度和频率的横向正弦抖动，用于量化稳定器的衰减"),
    ("zigzag", "锯齿折线，用于观察尖角处的平滑与圆角化"),
    ("circle", "一整圈圆"),
    ("spiral", "三圈向内收的螺旋"),
    ("dots", "六次原地点按，压力依次 0.1 → 1.0，用于看单个笔触的形状"),
    ("hold-move", "落笔后原地停 400 ms 再画直线，用于判断落笔瞬间是否出墨"),
];

fn smoothstep(u: f64) -> f64 {
    let u = u.clamp(0.0, 1.0);
    u * u * (3.0 - 2.0 * u)
}

/// 按 rate 对一条参数曲线采样。f(t_seconds) -> (x, y, p)。
fn sample(dur_s: f64, rate_hz: f64, f: impl Fn(f64) -> (f64, f64, f64)) -> Stroke {
    let n = (dur_s * rate_hz).round().max(1.0) as usize;
    let pts = (0..=n)
        .map(|k| {
            let t = k as f64 / rate_hz;
            let (x, y, p) = f(t.min(dur_s));
            Pt { t_us: (t * 1e6).round() as i64, x, y, p: p.clamp(0.0, 1.0) }
        })
        .collect();
    Stroke { pts }
}

pub fn build(name: &str, q: &Params) -> Option<Vec<Stroke>> {
    let (w, h) = (q.w, q.h);
    let (x0, x1, ym) = (MARGIN, w - MARGIN, h / 2.0);
    let len = x1 - x0;
    let dur = len / q.speed;
    let lo = 0.02;

    // 水平直线族：只有压力曲线和纵向偏移不同。
    let hline = |pf: &dyn Fn(f64) -> f64, yf: &dyn Fn(f64, f64) -> f64| {
        sample(dur, q.rate_hz, |t| {
            let u = t / dur;
            (x0 + len * u, yf(t, u), pf(u))
        })
    };
    let flat_y = |_t: f64, _u: f64| ym;

    let strokes = match name {
        "line" => vec![hline(&|_| q.pressure, &flat_y)],
        "ramp" => vec![hline(&|u| lo + (q.pressure - lo) * (1.0 - (2.0 * u - 1.0).abs()), &flat_y)],
        "ramp-up" => vec![hline(&|u| lo + (q.pressure - lo) * u, &flat_y)],
        "ramp-down" => vec![hline(&|u| lo + (q.pressure - lo) * (1.0 - u), &flat_y)],
        "flick" => vec![hline(
            &|u| q.pressure * smoothstep(u / 0.2) * smoothstep((1.0 - u) / 0.35),
            &flat_y,
        )],
        "jitter" => vec![hline(&|_| q.pressure, &|t, _| ym + q.amp * (2.0 * PI * q.freq * t).sin())],
        "zigzag" => {
            let amp = (h / 2.0 - MARGIN).min(80.0);
            let period = 240.0;
            vec![hline(&|_| q.pressure, &|_, u| {
                let phase = (len * u / period).fract();
                ym + amp * (4.0 * (phase - 0.5).abs() - 1.0)
            })]
        }
        "circle" => {
            let r = w.min(h) / 2.0 - MARGIN;
            let d = 2.0 * PI * r / q.speed;
            vec![sample(d, q.rate_hz, |t| {
                let a = 2.0 * PI * t / d;
                (w / 2.0 + r * a.cos(), h / 2.0 + r * a.sin(), q.pressure)
            })]
        }
        "spiral" => {
            let r0 = w.min(h) / 2.0 - MARGIN;
            let turns = 3.0;
            // 平均半径 0.6·r0，按它估算时长，线速度随半径略有变化。
            let d = 2.0 * PI * turns * 0.6 * r0 / q.speed;
            vec![sample(d, q.rate_hz, |t| {
                let u = t / d;
                let r = r0 * (1.0 - 0.8 * u);
                let a = 2.0 * PI * turns * u;
                (w / 2.0 + r * a.cos(), h / 2.0 + r * a.sin(), q.pressure)
            })]
        }
        "dots" => {
            let levels = [0.1, 0.25, 0.4, 0.6, 0.8, 1.0];
            levels
                .iter()
                .enumerate()
                .map(|(i, &p)| {
                    let x = x0 + len * (i as f64 + 0.5) / levels.len() as f64;
                    sample(0.08, q.rate_hz, move |_| (x, ym, p))
                })
                .collect()
        }
        "hold-move" => {
            let hold = 0.4;
            vec![sample(hold + dur, q.rate_hz, |t| {
                let u = ((t - hold) / dur).max(0.0);
                (x0 + len * u, ym, q.pressure)
            })]
        }
        _ => return None,
    };
    Some(strokes)
}

/// 单条、x 单调递增的用例才能做"按列"的粗细剖面分析。
pub fn is_horizontal(name: &str) -> bool {
    matches!(name, "line" | "ramp" | "ramp-up" | "ramp-down" | "flick" | "jitter" | "hold-move")
}
