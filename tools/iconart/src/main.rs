// SPDX-License-Identifier: GPL-3.0-or-later
//! iconart：生成纸笔（Pen & Paper）的应用图标。
//!
//! 画面是一张略微斜放、带柔和投影的纸，纸上随手放着一支深蓝水笔、一支白杆黄帽的涂改液笔、
//! 一支珊瑚红马克笔和一把木柄油漆刷（金属箍、扁刷毛、刷尖蘸着青绿色颜料）；背景透明。
//! 先用自带的 SDF 光栅器在 1024×1024 上画，再按面积平均缩小到各尺寸；
//! 24 像素及以下另画一版简化的（只有纸、水笔、刷子和马克笔，工具更粗，纸边对齐像素格）。
//!
//! 重新生成（输出是确定性的）：在 tools 目录下 `cargo run --release -p iconart`
//! 写出 src/app/assets/icon.ico（256…16 共 10 个尺寸）、icon-256.png、icon-1024.png。
//! 加 `--sheet DIR` 另外把各尺寸 PNG 和放大的对照表写到 DIR，方便检查小尺寸效果。

mod ico;
mod raster;
mod scene;

use std::path::{Path, PathBuf};

use raster::Canvas;

/// .ico 里的尺寸，从大到小
const SIZES: [usize; 10] = [256, 128, 96, 64, 48, 40, 32, 24, 20, 16];
/// 不超过这个尺寸时改用简化画面
const SMALL_MAX: usize = 24;
const ART: usize = 1024;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sheet_dir = args
        .iter()
        .position(|a| a == "--sheet")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    // tools/iconart → 仓库根目录 → src/app/assets
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("找不到仓库根目录");
    let assets = root.join("src").join("app").join("assets");
    std::fs::create_dir_all(&assets).expect("创建 assets 目录失败");

    let mut full = Canvas::new(ART);
    scene::full(&mut full);

    let mut images = Vec::new();
    for &s in &SIZES {
        let img = if s <= SMALL_MAX {
            let mut c = Canvas::new(ART);
            scene::small(&mut c, s);
            c.downscale(s)
        } else {
            full.downscale(s)
        };
        images.push((s, img.to_rgba8()));
    }

    let full8 = full.to_rgba8();
    ico::write_png(&assets.join("icon-1024.png"), &full8, ART, ART).expect("写 icon-1024.png 失败");
    let i256 = &images.iter().find(|(s, _)| *s == 256).expect("缺 256").1;
    ico::write_png(&assets.join("icon-256.png"), i256, 256, 256).expect("写 icon-256.png 失败");
    ico::write_ico(&assets.join("icon.ico"), &images).expect("写 icon.ico 失败");
    println!("已写出 {}", assets.join("icon.ico").display());

    if let Some(dir) = sheet_dir {
        std::fs::create_dir_all(&dir).expect("创建对照表目录失败");
        for (s, rgba) in &images {
            ico::write_png(&dir.join(format!("icon-{s}.png")), rgba, *s, *s).expect("写 PNG 失败");
        }
        write_sheets(&dir, &images);
        println!("对照表已写到 {}", dir.display());
    }
}

/// 在纯色底上合成一个图标，左上角放在 (ox, oy)，按整数倍 f 最近邻放大
fn blit(dst: &mut [u8], dw: usize, ox: usize, oy: usize, src: &[u8], s: usize, f: usize) {
    for y in 0..s * f {
        for x in 0..s * f {
            let si = ((y / f) * s + x / f) * 4;
            let a = src[si + 3] as f32 / 255.0;
            let di = ((oy + y) * dw + ox + x) * 3;
            for c in 0..3 {
                let v = src[si + c] as f32 * a + dst[di + c] as f32 * (1.0 - a);
                dst[di + c] = (v + 0.5) as u8;
            }
        }
    }
}

/// 两张对照表：sheet-zoom.png 把小尺寸放大到约 192 像素看细节；
/// sheet-1x.png 按原始像素大小排一排。上排浅色底、下排深色底，模拟浅色 / 深色任务栏。
fn write_sheets(dir: &Path, images: &[(usize, Vec<u8>)]) {
    let bgs = [[0xf3u8, 0xf3, 0xf3], [0x20, 0x20, 0x20]];

    let zoom: Vec<&(usize, Vec<u8>)> = images.iter().filter(|(s, _)| *s <= 64).collect();
    let cell = 208;
    let (w, h) = (cell * zoom.len(), cell * 2);
    let mut buf = vec![0u8; w * h * 3];
    for (row, bg) in bgs.iter().enumerate() {
        for y in row * cell..(row + 1) * cell {
            for x in 0..w {
                buf[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(bg);
            }
        }
        for (i, (s, rgba)) in zoom.iter().rev().enumerate() {
            let f = 192 / s;
            let o = (cell - s * f) / 2;
            blit(&mut buf, w, i * cell + o, row * cell + o, rgba, *s, f);
        }
    }
    write_rgb(&dir.join("sheet-zoom.png"), &buf, w, h);

    let gap = 12;
    let w = images.iter().map(|(s, _)| s + gap).sum::<usize>() + gap;
    let rowh = 256 + 2 * gap;
    let h = rowh * 2;
    let mut buf = vec![0u8; w * h * 3];
    for (row, bg) in bgs.iter().enumerate() {
        for y in row * rowh..(row + 1) * rowh {
            for x in 0..w {
                buf[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(bg);
            }
        }
        let mut x = gap;
        for (s, rgba) in images {
            blit(&mut buf, w, x, row * rowh + gap + (256 - s), rgba, *s, 1);
            x += s + gap;
        }
    }
    write_rgb(&dir.join("sheet-1x.png"), &buf, w, h);
}

fn write_rgb(path: &Path, rgb: &[u8], w: usize, h: usize) {
    let mut rgba = Vec::with_capacity(w * h * 4);
    for p in rgb.chunks(3) {
        rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
    }
    ico::write_png(path, &rgba, w, h).expect("写对照表失败");
}
