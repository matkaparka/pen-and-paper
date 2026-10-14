//! 把一次测试的原始记录和汇总指标写到输出目录。

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::cases::{self, Params};
use crate::inject::Phase;
use crate::util::{luma, percentile, Rect};
use crate::{FrameRec, SampleRec};

pub struct Run<'a> {
    pub case: &'a str,
    pub params: &'a Params,
    pub roi: Rect,
    pub exe: &'a str,
    pub title: &'a str,
    pub settle_ms: u64,
    pub w: usize,
    pub h: usize,
}

fn ms(us: i64) -> f64 {
    us as f64 / 1000.0
}

fn stats(mut v: Vec<f64>) -> (f64, f64, f64, usize) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (percentile(&v, 0.5), percentile(&v, 0.1), percentile(&v, 0.9), v.len())
}

struct Summary(Vec<(String, String)>);

impl Summary {
    fn num(&mut self, key: &str, v: f64) {
        let s = if v.is_finite() { format!("{v:.3}") } else { "null".into() };
        self.0.push((key.into(), s));
    }
    fn int(&mut self, key: &str, v: i64) {
        self.0.push((key.into(), v.to_string()));
    }
    fn text(&mut self, key: &str, v: &str) {
        self.0.push((key.into(), format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))));
    }
    fn write(&self, path: &Path) -> io::Result<()> {
        let mut f = BufWriter::new(File::create(path)?);
        writeln!(f, "{{")?;
        for (i, (k, v)) in self.0.iter().enumerate() {
            writeln!(f, "  \"{k}\": {v}{}", if i + 1 < self.0.len() { "," } else { "" })?;
        }
        writeln!(f, "}}")
    }
}

pub fn write_all(
    dir: &Path,
    run: &Run,
    recs: &[SampleRec],
    frames: &[FrameRec],
    refb: &[u8],
    after: &[u8],
) -> io::Result<()> {
    let mut f = BufWriter::new(File::create(dir.join("samples.csv"))?);
    writeln!(f, "stroke,phase,contact_idx,t_target_us,t_before_us,t_after_us,x,y,pressure")?;
    for r in recs {
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{}",
            r.stroke, r.phase.name(), r.contact_idx, r.t_target_us, r.t_before_us, r.t_after_us, r.x, r.y, r.pressure
        )?;
    }
    drop(f);

    let mut f = BufWriter::new(File::create(dir.join("frames.csv"))?);
    writeln!(f, "present_us,accumulated,changed_px,dark_px,front")?;
    for fr in frames {
        writeln!(f, "{},{},{},{},{}", fr.present_us, fr.accumulated, fr.changed_px, fr.dark_px, fr.front)?;
    }
    drop(f);

    let mut s = Summary(Vec::new());
    s.text("tool", concat!("strokebench ", env!("CARGO_PKG_VERSION")));
    s.text("case", run.case);
    s.text("target_exe", run.exe);
    s.text("target_title", run.title);
    s.text("rect", &format!("{},{},{},{}", run.roi.x, run.roi.y, run.roi.w, run.roi.h));
    s.num("rate_hz", run.params.rate_hz);
    s.num("speed_px_s", run.params.speed);
    s.num("pressure", run.params.pressure);
    s.num("jitter_amp_px", run.params.amp);
    s.num("jitter_freq_hz", run.params.freq);
    s.int("settle_ms", run.settle_ms as i64);
    s.int(
        "unix_time",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
    );

    let contact: Vec<&SampleRec> = recs.iter().filter(|r| r.contact_idx >= 0).collect();
    let n_strokes = recs.iter().map(|r| r.stroke).max().map_or(0, |m| m + 1);
    s.int("strokes", n_strokes as i64);
    s.int("contact_samples", contact.len() as i64);
    if contact.is_empty() {
        return s.write(&dir.join("summary.json"));
    }

    // 注入本身的质量：实际发出时刻偏离计划多少，一次调用花多久。
    let late: Vec<f64> = recs.iter().map(|r| (r.t_before_us - r.t_target_us) as f64).collect();
    let call: Vec<f64> = recs.iter().map(|r| (r.t_after_us - r.t_before_us) as f64).collect();
    s.num("inject_late_us_mean", late.iter().sum::<f64>() / late.len() as f64);
    s.num("inject_late_us_max", late.iter().cloned().fold(f64::MIN, f64::max));
    s.num("inject_call_us_mean", call.iter().sum::<f64>() / call.len() as f64);

    let t_down = contact[0].t_after_us;
    let t_last = contact.last().unwrap().t_after_us;
    let t_up = recs.iter().rev().find(|r| r.phase == Phase::Up).map_or(t_last, |r| r.t_after_us);
    s.num("stroke_duration_ms", ms(t_last - t_down));

    // 落笔之前（悬停阶段）画面本来就有的变化量，用作出墨判断的底噪。
    let pre_dark = frames.iter().filter(|f| f.present_us <= t_down).map(|f| f.dark_px).max().unwrap_or(0);
    let pre_changed = frames.iter().filter(|f| f.present_us <= t_down).map(|f| f.changed_px).max().unwrap_or(0);
    let after_down = || frames.iter().filter(|f| f.present_us > t_down);
    let first_change = after_down().find(|f| f.changed_px > pre_changed);
    let first_ink = after_down().find(|f| f.dark_px >= pre_dark + 6);
    s.num("first_change_ms", first_change.map_or(f64::NAN, |f| ms(f.present_us - t_down)));
    s.num("first_ink_ms", first_ink.map_or(f64::NAN, |f| ms(f.present_us - t_down)));

    // 笔画进行中画面的刷新情况。Desktop Duplication 只在桌面图像变化时出帧，
    // 所以这段时间内的帧数就是目标程序实际更新画面的次数。
    let during: Vec<&FrameRec> = frames.iter().filter(|f| f.present_us > t_down && f.present_us <= t_up).collect();
    s.int("frames_total", frames.len() as i64);
    s.int("frames_during_stroke", during.len() as i64);
    if t_up > t_down {
        s.num("update_rate_hz", during.len() as f64 * 1e6 / (t_up - t_down) as f64);
    }
    // 漏帧看帧间隔：间隔接近刷新周期的两倍才是真的少了一帧。
    // （系统报告的 AccumulatedFrames 在画面没变时也会是 2，不能当漏帧数用，只原样留在 frames.csv 里。）
    let mut gaps: Vec<f64> = during.windows(2).map(|p| ms(p[1].present_us - p[0].present_us)).collect();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s.num("frame_interval_ms_median", percentile(&gaps, 0.5));
    s.num("frame_interval_ms_p95", percentile(&gaps, 0.95));
    s.num("frame_interval_ms_max", gaps.last().copied().unwrap_or(f64::NAN));

    // 沿路径的累计长度，用来把"落后几个样本"换算成"落后多少像素"。
    let mut arc = vec![0.0f64; contact.len()];
    for i in 1..contact.len() {
        let (dx, dy) = ((contact[i].x - contact[i - 1].x) as f64, (contact[i].y - contact[i - 1].y) as f64);
        let jump = contact[i].stroke != contact[i - 1].stroke;
        arc[i] = arc[i - 1] + if jump { 0.0 } else { (dx * dx + dy * dy).sqrt() };
    }

    // 跟随延迟：某一帧里最靠前的墨迹，是多久以前注入的那个样本画出来的。
    // 只取单笔画的中段，避开起笔和收笔。
    if n_strokes == 1 && t_last > t_down {
        let (lo, hi) = (t_down + (t_last - t_down) / 5, t_down + (t_last - t_down) * 9 / 10);
        let (mut lag_ms, mut lag_px) = (Vec::new(), Vec::new());
        for fr in frames.iter().filter(|f| f.front >= 0 && f.present_us >= lo && f.present_us <= hi) {
            let front = fr.front as usize;
            lag_ms.push(ms(fr.present_us - contact[front].t_after_us));
            let pen = contact.partition_point(|c| c.t_after_us <= fr.present_us).saturating_sub(1);
            lag_px.push(arc[pen.max(front)] - arc[front]);
        }
        let (m, p10, p90, n) = stats(lag_ms);
        s.int("track_frames", n as i64);
        s.num("track_lag_ms_median", m);
        s.num("track_lag_ms_p10", p10);
        s.num("track_lag_ms_p90", p90);
        let (m, p10, p90, _) = stats(lag_px);
        s.num("track_lag_px_median", m);
        s.num("track_lag_px_p10", p10);
        s.num("track_lag_px_p90", p90);
    }

    if run.case == "line" && t_last > t_down {
        area_lag(run, &contact, frames, refb, after, pre_dark, t_down, t_last, &mut s);
    }

    // 收笔：墨迹最终到了哪里，抬笔后多久才停止前进。
    let final_front = frames.last().map_or(-1, |f| f.front);
    s.int("final_front_idx", final_front);
    s.int("last_contact_idx", contact.len() as i64 - 1);
    if final_front >= 0 {
        s.num("tail_missing_px", arc[contact.len() - 1] - arc[final_front as usize]);
        if let Some(fr) = frames.iter().find(|f| f.front == final_front) {
            s.num("ink_complete_after_up_ms", ms(fr.present_us - t_up));
        }
    }
    s.int("final_dark_px", frames.last().map_or(0, |f| f.dark_px as i64));

    if cases::is_horizontal(run.case) {
        profile(dir, run, &contact, refb, after, &mut s)?;
    }

    s.write(&dir.join("summary.json"))?;
    for (k, v) in &s.0 {
        println!("{k} = {v}");
    }
    Ok(())
}

/// 面积法的跟随延迟，只用于恒压水平直线：还没画出来的长度 = 还差的墨迹面积 ÷ 线宽。
///
/// 上面按"样本点周围是否有墨"算出的 track_lag 有系统偏差：笔刷有半径，墨迹的前缘比最新一个笔触的
/// 中心靠前半个线宽，笔速越慢这段距离折合的时间越长。面积法不依赖逐点判定，没有这个偏差。
#[allow(clippy::too_many_arguments)]
fn area_lag(
    run: &Run,
    contact: &[&SampleRec],
    frames: &[FrameRec],
    refb: &[u8],
    after: &[u8],
    base_dark: u32,
    t_down: i64,
    t_last: i64,
    s: &mut Summary,
) {
    let (w, h) = (run.w, run.h);
    let (x0, x1) = ((contact[0].x - run.roi.x) as usize, (contact[contact.len() - 1].x - run.roi.x) as usize);
    if x1 <= x0 + 10 || x1 >= w {
        return;
    }
    // 中段每一列有多少个有墨像素，取中位数当线宽。
    let mut cols: Vec<f64> = ((x0 + (x1 - x0) * 3 / 10)..(x0 + (x1 - x0) * 7 / 10))
        .map(|x| {
            (0..h)
                .filter(|&y| {
                    let i = (y * w + x) * 4;
                    luma(refb[i], refb[i + 1], refb[i + 2]) - luma(after[i], after[i + 1], after[i + 2]) > crate::DARK_DROP
                })
                .count() as f64
        })
        .collect();
    cols.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let w_dark = percentile(&cols, 0.5);
    s.num("line_dark_width_px", w_dark);
    let Some(last) = frames.last() else { return };
    if w_dark < 3.0 {
        return;
    }
    let final_dark = last.dark_px as f64;
    // 把面积换算成长度时用"中段各列墨量的平均值"：
    // - 不用中位数，因为视图旋转后线的边缘是锯齿状的，各列墨量在两个整数之间跳，中位数会偏；
    // - 不用"总面积 ÷ 总长度"，因为起笔渐细的那一段墨量少，会把平均值拉低，
    //   稳定器档位越高渐细越长，偏差越大（档位 15 时曾把延迟高估约 10 毫秒）。
    let per_px = cols.iter().sum::<f64>() / cols.len().max(1) as f64;
    s.num("line_dark_per_px_mean", per_px);

    // 笔在某一时刻的 x，以及笔经过某个 x 的时刻；恒速直线上两者都是单调的。
    let pen_x_at = |t: i64| -> f64 {
        let k = contact.partition_point(|c| c.t_after_us <= t).clamp(1, contact.len() - 1);
        let (a, b) = (contact[k - 1], contact[k]);
        let u = ((t - a.t_after_us) as f64 / (b.t_after_us - a.t_after_us).max(1) as f64).clamp(0.0, 1.0);
        a.x as f64 + (b.x - a.x) as f64 * u
    };
    let pen_t_at = |x: f64| -> f64 {
        let k = contact.partition_point(|c| (c.x as f64) <= x).clamp(1, contact.len() - 1);
        let (a, b) = (contact[k - 1], contact[k]);
        let u = ((x - a.x as f64) / ((b.x - a.x) as f64).max(1e-6)).clamp(0.0, 1.0);
        a.t_after_us as f64 + (b.t_after_us - a.t_after_us) as f64 * u
    };

    let (lo, hi) = (t_down + (t_last - t_down) / 5, t_down + (t_last - t_down) * 9 / 10);
    let (mut lag_ms, mut lag_px) = (Vec::new(), Vec::new());
    for fr in frames.iter().filter(|f| f.present_us >= lo && f.present_us <= hi) {
        // 笔画进行中，画面里除了墨迹还有程序自绘的光标，它的像素数在落笔前已经量到（base_dark）。
        let missing = (final_dark + base_dark as f64 - fr.dark_px as f64) / per_px;
        let ink_x = contact[contact.len() - 1].x as f64 - missing;
        lag_px.push(pen_x_at(fr.present_us) - ink_x);
        lag_ms.push((fr.present_us as f64 - pen_t_at(ink_x)) / 1000.0);
    }
    let (m, p10, p90, n) = stats(lag_ms);
    s.int("area_lag_frames", n as i64);
    s.num("area_lag_ms_median", m);
    s.num("area_lag_ms_p10", p10);
    s.num("area_lag_ms_p90", p90);
    s.num("area_lag_px_median", stats(lag_px).0);
    s.int("cursor_dark_px_before_down", base_dark as i64);
}

/// 按列的笔迹剖面：每一列的墨量（折合成"满黑像素数"，即等效粗细）、最深处的浓度、
/// 墨迹重心的纵坐标，以及画到这一列时注入的压力和纵坐标。
fn profile(dir: &Path, run: &Run, contact: &[&SampleRec], refb: &[u8], after: &[u8], s: &mut Summary) -> io::Result<()> {
    let (w, h) = (run.w, run.h);
    let mut f = BufWriter::new(File::create(dir.join("profile.csv"))?);
    writeln!(f, "x,width_px,max_cov,centroid_y,in_y,in_pressure")?;

    let mut rows: Vec<(usize, f64, f64, f64, f64)> = Vec::new(); // x, width, centroid, in_y, in_p
    let mut k = 0usize;
    for x in 0..w {
        let (mut sum, mut max_cov, mut moment) = (0.0f64, 0.0f64, 0.0f64);
        for y in 0..h {
            let i = (y * w + x) * 4;
            let l0 = luma(refb[i], refb[i + 1], refb[i + 2]);
            let l1 = luma(after[i], after[i + 1], after[i + 2]);
            let cov = ((l0 - l1) as f64 / l0.max(1) as f64).clamp(0.0, 1.0);
            if cov > 0.02 {
                sum += cov;
                moment += cov * y as f64;
                max_cov = max_cov.max(cov);
            }
        }
        // 这一列对应的输入：屏幕 x 不超过该列的最后一个接触样本。
        let sx = run.roi.x + x as i32;
        while k + 1 < contact.len() && contact[k + 1].x <= sx {
            k += 1;
        }
        let inside = sx >= contact[0].x && sx <= contact[contact.len() - 1].x;
        let (in_y, in_p) = if inside {
            ((contact[k].y - run.roi.y) as f64, contact[k].pressure as f64 / 1024.0)
        } else {
            (f64::NAN, f64::NAN)
        };
        let centroid = if sum > 0.0 { moment / sum } else { f64::NAN };
        writeln!(f, "{x},{sum:.3},{max_cov:.3},{centroid:.3},{in_y:.1},{in_p:.4}")?;
        rows.push((x, sum, centroid, in_y, in_p));
    }

    let inked: Vec<&(usize, f64, f64, f64, f64)> = rows.iter().filter(|r| r.1 > 0.5).collect();
    let (in_x0, in_x1) = (contact[0].x - run.roi.x, contact[contact.len() - 1].x - run.roi.x);
    s.int("in_x_start", in_x0 as i64);
    s.int("in_x_end", in_x1 as i64);
    if let (Some(a), Some(b)) = (inked.first(), inked.last()) {
        s.int("ink_x_start", a.0 as i64);
        s.int("ink_x_end", b.0 as i64);
        s.num("ink_width_px_max", inked.iter().map(|r| r.1).fold(0.0, f64::max));
        // 中间 60% 的平均粗细，以及墨迹重心相对输入纵坐标的横向偏差（稳定器会把它压小）。
        let (lo, hi) = (in_x0 as f64 + 0.2 * (in_x1 - in_x0) as f64, in_x0 as f64 + 0.8 * (in_x1 - in_x0) as f64);
        let mid: Vec<&&(usize, f64, f64, f64, f64)> =
            inked.iter().filter(|r| r.0 as f64 >= lo && r.0 as f64 <= hi && r.3.is_finite()).collect();
        if !mid.is_empty() {
            let n = mid.len() as f64;
            s.num("ink_width_px_mid_mean", mid.iter().map(|r| r.1).sum::<f64>() / n);
            let rms = |v: Vec<f64>| {
                let m = v.iter().sum::<f64>() / v.len() as f64;
                (v.iter().map(|a| (a - m) * (a - m)).sum::<f64>() / v.len() as f64).sqrt()
            };
            s.num("in_y_rms_px", rms(mid.iter().map(|r| r.3).collect()));
            s.num("ink_centroid_y_rms_px", rms(mid.iter().map(|r| r.2).collect()));
        }
    }
    Ok(())
}
