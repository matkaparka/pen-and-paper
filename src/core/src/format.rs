// SPDX-License-Identifier: GPL-3.0-or-later
//! 纸笔自己的文件格式（`.zhibi`）：一个 ZIP 压缩包，里面是一份 JSON 说明和几张 PNG。
//! 力求简单、公开，别的程序用一小段脚本就能读。
//!
//! - `document.json`：`format`（固定为 `"zhibi"`）、`version`、画布的 `width` `height` `dpi`、
//!   背景色 `background`、当前图层的序号 `active`，以及自下而上的图层列表 `layers`。每个图层有
//!   `name`、`opacity`（0 到 1）、`visible`、`blend`、图像在画布上占的矩形 `x` `y` `width` `height`
//!   和图像的文件名 `file`；空图层的 `file` 是 `null`，矩形大小为零。
//!   版本 2 起（0.3）每个图层还有：`kind`（`"pixels"`、`"folder"`、`"fill"`）、嵌套深度 `depth`
//!   （0 是最外层；文件夹里面的东西是紧挨在它下面、深度比它大的那一串）、`pass_through`；
//!   文件夹有 `open`；填充图层有 `fill`（`{"type": "solid", "color": "#RRGGBB"}`，或者本程序还不会画的
//!   `{"type": "other"}`），从 PSD 读来的填充图层另有 `psd`（原始描述符块的 `key` 和 Base64 的 `data`）；
//!   有蒙版的有 `mask`：`x` `y` `width` `height` `file`（同上）、矩形以外的值 `default`（`"white"` 或
//!   `"black"`）和 `disabled`。有图层样式的有 `style`：PS 原来的块 `psd`（`key` 和 Base64 的 `data` 的列表）、
//!   `enabled`、文件的全局光 `light`（角度、高度，没有就是 `null`）；读的时候照 PSD 的样子重新解析。
//!   PS 的画板（组）有 `artboard`：原来的块 `psd`（同上），读的时候重新解析。
//!   填充不透明度不是 1 的有 `fill_opacity`（0 到 1），透明形状图层关着的有 `"shapes": false`。
//!   图层、蒙版的矩形可以伸出画布（不超过画布四周各一个画布的宽和高）。
//! - `layers/NNNN.png`：一个图层的像素，16 位 RGBA、非预乘透明度（PNG 的规定），只含有内容的那块矩形。
//! - `masks/NNNN.png`：一个图层的蒙版，16 位灰度。
//! - `merged.png`：合成好的整张画布，8 位 RGB，给别的程序预览用。
//! - `thumbnail.png`：同上，缩小到长边不超过 256 像素。
//!
//! 读的时候只用 `document.json` 和它提到的图层图像，不认识的字段一律忽略。

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value};
use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::document::{BlendMode, Document, Fill, Kind, Layer, LayerProps, Mask, Paint, MAX_SIDE, Style};
use crate::lang::tr;
use crate::tile::{GrayMap, Tile, TileMap, TILE};
use crate::trf;
use crate::{Rect, ONE};

/// 文件扩展名（不含点）
pub const EXTENSION: &str = "zhibi";

/// `document.json` 里 `format` 字段的值。
const FORMAT: &str = "zhibi";
/// 写出的格式版本，也是能读的最高版本。
const VERSION: u64 = 2;
const DOCUMENT_JSON: &str = "document.json";
const MERGED_PNG: &str = "merged.png";
const THUMBNAIL_PNG: &str = "thumbnail.png";
/// 缩略图长边的上限（像素）。
const THUMBNAIL_SIDE: usize = 256;
/// `document.json` 解压后的大小上限。正常只有几 KB，超出的只可能是坏文件或者压缩炸弹。
const MAX_JSON_BYTES: u64 = 16 << 20;
/// 16 位通道里的 1.0。
const MAX16: u64 = 65535;
/// 15 位定点里的 1.0，换算像素时用的宽类型。
const ONE64: u64 = ONE as u64;

// ---- 像素换算 ----
//
// 分块里是 15 位、预乘的，PNG 要 16 位、非预乘的。两个方向都四舍五入，来回一趟一位不差：
// 正向的舍入误差不超过 16 位的半格，换回去要乘以 a/65535（或 32768/65535），都不超过 1/2，
// 于是落在 15 位的四分之一格以内，再四舍五入就回到原值。

/// 四舍五入的整数除法。
#[inline]
fn div_round(n: u64, d: u64) -> u64 {
    (2 * n + d) / (2 * d)
}

/// 分块像素（15 位、预乘）→ PNG 像素（16 位、非预乘）。
#[inline]
fn to_straight(px: [u16; 4]) -> [u16; 4] {
    // 正常的像素满足 颜色 ≤ 透明度 ≤ ONE。不满足的在非预乘格式里没法表示，夹到最近的合法值。
    let a = (px[3] as u64).min(ONE64);
    if a == 0 {
        return [0; 4];
    }
    let ch = |c: u16| {
        let n = (c as u64).min(a) * MAX16;
        // 不透明的像素最多，单独走移位，省掉一次真正的除法。
        (if a == ONE64 { (n + ONE64 / 2) >> 15 } else { div_round(n, a) }) as u16
    };
    [ch(px[0]), ch(px[1]), ch(px[2]), div_round(a * MAX16, ONE64) as u16]
}

/// PNG 像素（16 位、非预乘）→ 分块像素（15 位、预乘）。
#[inline]
fn to_premultiplied(px: [u16; 4]) -> [u16; 4] {
    let a = div_round(px[3] as u64 * ONE64, MAX16);
    let ch = |c: u16| div_round(c as u64 * a, MAX16) as u16;
    [ch(px[0]), ch(px[1]), ch(px[2]), a as u16]
}

// ---- 保存 ----

/// 把文档存成 `.zhibi` 文件。
///
/// 先写进同一目录下的临时文件，完整落盘之后再改名顶替 `path`，所以中途失败不会留下半截文件，
/// 原来的文件也原样还在。不会把文档标成"已保存"（`mark_saved`），那是调用方在成功之后做的事。
pub fn save(doc: &Document, path: &Path) -> io::Result<()> {
    if !(1..=MAX_SIDE).contains(&doc.width) || !(1..=MAX_SIDE).contains(&doc.height) {
        let (w, h) = (doc.width, doc.height);
        let msg = trf!("画布尺寸无效：{w}×{h}", "Invalid canvas size: {w}×{h}");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
    }
    let (tmp, file) = create_temp(path)?;
    let result = write_archive(doc, file).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 在 `path` 旁边新建一个临时文件。放在同一目录，改名才是原地替换而不是跨盘复制。
fn create_temp(path: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, tr("保存路径里没有文件名", "The save path has no file name")))?;
    let mut last = None;
    for _ in 0..100 {
        let mut tmp_name = OsString::from(name);
        tmp_name.push(format!(".{}-{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let tmp = path.with_file_name(tmp_name);
        // create_new：撞上已有的文件（比如上次崩溃留下的）就换个名字，绝不覆盖别人的东西。
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other(tr("无法创建临时文件", "Could not create a temporary file"))))
}

/// 把整个压缩包写进 `file` 并落盘。`file` 在返回时关闭，之后才能改名或删除。
fn write_archive(doc: &Document, file: File) -> io::Result<()> {
    // 伸出画布的内容照存（实现方案 §3.15）。
    let keep = doc.keep_bounds();
    let rects: Vec<Rect> = doc.layers().iter().map(|l| content_bounds(&l.tiles, keep)).collect();
    let names: Vec<Option<String>> =
        rects.iter().enumerate().map(|(i, r)| (!r.is_empty()).then(|| format!("layers/{i:04}.png"))).collect();
    let mask_rects: Vec<Rect> = doc.layers().iter().map(|l| l.mask.as_ref().map_or(Rect::EMPTY, |m| mask_bounds(&m.tiles, keep))).collect();
    let layers: Vec<Value> = doc
        .layers()
        .iter()
        .enumerate()
        .zip(rects.iter().zip(&names))
        .map(|((i, layer), (r, name))| {
            let mut v = json!({
                "name": layer.props.name,
                "opacity": opacity_number(layer.props.opacity),
                "visible": layer.props.visible,
                "blend": layer.props.blend.id(),
                "lock_alpha": layer.props.lock_alpha,
                "locked": layer.props.locked,
                "clip": layer.props.clip,
                "pass_through": layer.props.pass_through,
                "depth": layer.depth,
                "x": r.x0,
                "y": r.y0,
                "width": r.x1 - r.x0,
                "height": r.y1 - r.y0,
                "file": name,
            });
            if layer.props.fill < 1.0 {
                v["fill_opacity"] = json!(opacity_number(layer.props.fill));
            }
            if !layer.props.shapes {
                v["shapes"] = json!(false);
            }
            match &layer.kind {
                Kind::Pixels => v["kind"] = json!("pixels"),
                Kind::Folder { open } => {
                    v["kind"] = json!("folder");
                    v["open"] = json!(open);
                }
                Kind::Fill(fill) => {
                    v["kind"] = json!("fill");
                    v["fill"] = match fill.paint {
                        Paint::Solid(c) => json!({ "type": "solid", "color": format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2]) }),
                        Paint::Other => json!({ "type": "other" }),
                    };
                    if let Some((key, data)) = &fill.psd {
                        v["psd"] = json!({ "key": String::from_utf8_lossy(key), "data": base64(data) });
                    }
                }
            }
            if let Some(board) = &layer.artboard {
                let blocks: Vec<Value> = board.psd.iter().map(|(k, d)| json!({ "key": String::from_utf8_lossy(k), "data": base64(d) })).collect();
                v["artboard"] = json!({ "psd": blocks });
            }
            if let Some(st) = &layer.style {
                let blocks: Vec<Value> = st.psd.iter().map(|(k, d)| json!({ "key": String::from_utf8_lossy(k), "data": base64(d) })).collect();
                v["style"] = json!({ "psd": blocks, "enabled": st.enabled, "light": st.light.map(|(a, h)| [a, h]) });
            }
            if let Some(m) = &layer.mask {
                let r = mask_rects[i];
                v["mask"] = json!({
                    "x": r.x0,
                    "y": r.y0,
                    "width": r.x1 - r.x0,
                    "height": r.y1 - r.y0,
                    "file": (!r.is_empty()).then(|| format!("masks/{i:04}.png")),
                    "default": if m.default as u32 >= ONE / 2 { "white" } else { "black" },
                    "disabled": m.disabled,
                });
            }
            v
        })
        .collect();
    let meta = json!({
        "format": FORMAT,
        "version": VERSION,
        "width": doc.width,
        "height": doc.height,
        "dpi": doc.dpi,
        "background": doc.background,
        "active": doc.active(),
        "layers": layers,
    });

    // 所有条目都原样存放、不再压缩：PNG 自己已经压过了；document.json 很小，放在最前面又不压缩，
    // 别的工具不解包也能认出这是什么文件。
    // 每个条目都先在内存里编码好再整块写进去。直接让 PNG 编码器往压缩包里写的话，它收尾时的
    // 写入错误会在析构里被悄悄吞掉；写进 Vec 不会失败，写进文件的那一步出错就能如实报出来。
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let mut add = |name: &str, data: &[u8]| -> io::Result<()> {
        zip.start_file(name, stored)?;
        zip.write_all(data)
    };
    add(DOCUMENT_JSON, serde_json::to_string_pretty(&meta)?.as_bytes())?;
    for ((layer, r), name) in doc.layers().iter().zip(&rects).zip(&names) {
        if let Some(name) = name {
            add(name, &encode_layer(&layer.tiles, *r)?)?;
        }
    }
    for (i, (layer, r)) in doc.layers().iter().zip(&mask_rects).enumerate() {
        if let (Some(m), false) = (&layer.mask, r.is_empty()) {
            add(&format!("masks/{i:04}.png"), &encode_mask(m, *r)?)?;
        }
    }
    let (w, h) = (doc.width as usize, doc.height as usize);
    let rgb = doc.flatten_rgb();
    add(MERGED_PNG, &encode_rgb8(w, h, &rgb)?)?;
    let (thumb, tw, th) = thumbnail(&rgb, w, h);
    drop(rgb);
    add(THUMBNAIL_PNG, &encode_rgb8(tw, th, &thumb)?)?;

    let file = zip.finish()?.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

/// 不透明度在 JSON 里的数值。把 f32 直接转成 f64 写出来会是 0.30000001192092896 这种样子，
/// 所以先试最短的十进制写法（0.3）：只要它按 f64 读进来再转回 f32 还是原值就用它。
fn opacity_number(v: f32) -> f64 {
    // 合成时非数当 0 用，这里保持一致。
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    match v.to_string().parse::<f64>() {
        Ok(short) if short as f32 == v => short,
        _ => v as f64,
    }
}

/// 图层在矩形 `area`（图层能放到的范围）之内、不是全透明的像素的包围盒；没有这样的像素就是空矩形。
fn content_bounds(tiles: &TileMap, area: Rect) -> Rect {
    if area.is_empty() {
        return Rect::EMPTY;
    }
    let (tx0, ty0) = (area.x0.div_euclid(TILE), area.y0.div_euclid(TILE));
    let (tx1, ty1) = ((area.x1 - 1).div_euclid(TILE), (area.y1 - 1).div_euclid(TILE));
    let mut bounds = Rect::EMPTY;
    for (tx, ty) in tiles.coords() {
        // 先筛掉范围外的分块，下面乘 TILE 才不会溢出。
        if tx < tx0 || ty < ty0 || tx > tx1 || ty > ty1 {
            continue;
        }
        let Some(tile) = tiles.get((tx, ty)) else { continue };
        let (ox, oy) = (tx * TILE, ty * TILE);
        let clip = Rect { x0: ox, y0: oy, x1: ox + TILE, y1: oy + TILE }.intersect(area);
        // 已经整个落在包围盒里的分块不会再把它撑大，不用逐像素看。
        if bounds.union(clip) == bounds {
            continue;
        }
        for y in clip.y0..clip.y1 {
            let alpha = |x: i32| tile.0[(((y - oy) * TILE + (x - ox)) * 4 + 3) as usize];
            let Some(first) = (clip.x0..clip.x1).find(|&x| alpha(x) != 0) else { continue };
            let last = (clip.x0..clip.x1).rev().find(|&x| alpha(x) != 0).unwrap_or(first);
            bounds = bounds.union(Rect { x0: first, y0: y, x1: last + 1, y1: y + 1 });
        }
    }
    bounds
}

/// 把图层在矩形 `r`（非空，可以伸出画布）里的像素编码成 16 位 RGBA 的 PNG。
/// 一行一行地换算和压缩，不用整幅的 16 位缓冲区。
fn encode_layer(tiles: &TileMap, r: Rect) -> io::Result<Vec<u8>> {
    let width = (r.x1 - r.x0) as usize;
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width as u32, (r.y1 - r.y0) as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Sixteen);
    // 保存要快：大画布上一个图层就有上百 MB 的原始数据，所以用最快的压缩级别。
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header()?;
    let mut stream = writer.stream_writer()?;

    let (tx0, tx1) = (r.x0.div_euclid(TILE), (r.x1 - 1).div_euclid(TILE));
    let mut row = vec![0u8; width * 8];
    // 当前这一排分块；同一排的 64 行共用，不必每行都去查表。
    let mut strip: Vec<Option<&Tile>> = Vec::new();
    for y in r.y0..r.y1 {
        let ty = y.div_euclid(TILE);
        if y == r.y0 || y.rem_euclid(TILE) == 0 {
            strip.clear();
            strip.extend((tx0..=tx1).map(|tx| tiles.get((tx, ty)).map(|t| t.as_ref())));
        }
        for (tx, tile) in (tx0..=tx1).zip(&strip) {
            let (x0, x1) = ((tx * TILE).max(r.x0), ((tx + 1) * TILE).min(r.x1));
            let dst = &mut row[((x0 - r.x0) * 8) as usize..((x1 - r.x0) * 8) as usize];
            let Some(tile) = tile else {
                dst.fill(0);
                continue;
            };
            let at = (((y - ty * TILE) * TILE + (x0 - tx * TILE)) * 4) as usize;
            for (px, o) in tile.0[at..].as_chunks::<4>().0.iter().zip(dst.as_chunks_mut::<8>().0) {
                for (bytes, v) in o.as_chunks_mut::<2>().0.iter_mut().zip(to_straight(*px)) {
                    *bytes = v.to_be_bytes();
                }
            }
        }
        stream.write_all(&row)?;
    }
    stream.finish()?;
    writer.finish()?;
    Ok(out)
}

/// 蒙版里存着分块的范围（对齐到分块，裁到图层能放到的范围里）；范围外都是默认值。
fn mask_bounds(tiles: &GrayMap, keep: Rect) -> Rect {
    tiles.coords().fold(Rect::EMPTY, |r, (tx, ty)| r.union(Rect { x0: tx * TILE, y0: ty * TILE, x1: (tx + 1) * TILE, y1: (ty + 1) * TILE }.intersect(keep)))
}

/// 把蒙版在矩形 `r`（非空）里的值编码成 16 位灰度的 PNG。15 位换到 16 位四舍五入，来回一趟一位不差。
fn encode_mask(mask: &Mask, r: Rect) -> io::Result<Vec<u8>> {
    let width = (r.x1 - r.x0) as usize;
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width as u32, (r.y1 - r.y0) as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Sixteen);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header()?;
    let mut stream = writer.stream_writer()?;
    let mut row = vec![0u8; width * 2];
    for y in r.y0..r.y1 {
        for (k, x) in (r.x0..r.x1).enumerate() {
            let (tx, ty) = (x.div_euclid(TILE), y.div_euclid(TILE));
            let v = mask.tiles.get((tx, ty)).map_or(mask.default, |t| t[((y - ty * TILE) * TILE + (x - tx * TILE)) as usize]);
            let v16 = div_round((v as u64).min(ONE64) * MAX16, ONE64) as u16;
            row[k * 2..k * 2 + 2].copy_from_slice(&v16.to_be_bytes());
        }
        stream.write_all(&row)?;
    }
    stream.finish()?;
    writer.finish()?;
    Ok(out)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准的 Base64（带补齐的等号）。只用来存从 PSD 读来的那一小段原始数据，不值得为它加一个依赖。
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (k, &b)| n | (b as u32) << (16 - 8 * k));
        for k in 0..4 {
            out.push(if k <= chunk.len() { BASE64[(n >> (18 - 6 * k) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// `base64` 反过来。不是合法的 Base64 时返回 None。
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut n, mut bits) = (0u32, 0);
    for c in text.bytes() {
        n = n << 6 | BASE64.iter().position(|&b| b == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
        }
    }
    Some(out)
}

/// 8 位 RGB 的 PNG（合成图和缩略图）。
fn encode_rgb8(width: usize, height: usize, rgb: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(rgb)?;
    writer.finish()?;
    Ok(out)
}

/// 把 8 位 RGB 图像缩小到长边不超过 `THUMBNAIL_SIDE`：每个目标像素取它盖住的那一块源像素的平均值。
/// 本来就不超过的原样返回。返回像素和新的宽、高。
fn thumbnail(rgb: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let long = w.max(h);
    if long <= THUMBNAIL_SIDE {
        return (rgb.to_vec(), w, h);
    }
    // 短边至少留一个像素。tw ≤ w、th ≤ h，所以下面每一块至少有一个源像素。
    let (tw, th) = ((w * THUMBNAIL_SIDE / long).max(1), (h * THUMBNAIL_SIDE / long).max(1));
    let mut out = Vec::with_capacity(tw * th * 3);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, (ty + 1) * h / th);
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, (tx + 1) * w / tw);
            let mut sum = [0u32; 3];
            for y in y0..y1 {
                for px in rgb[(y * w + x0) * 3..(y * w + x1) * 3].as_chunks::<3>().0 {
                    for (s, &v) in sum.iter_mut().zip(px) {
                        *s += v as u32;
                    }
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            out.extend(sum.map(|s| ((s + n / 2) / n) as u8));
        }
    }
    (out, tw, th)
}

// ---- 读取 ----

/// `document.json` 的内容，已经检查过。
struct Header {
    width: i32,
    height: i32,
    dpi: Option<u32>,
    background: [u8; 3],
    active: Option<usize>,
    layers: Vec<LayerEntry>,
}

struct LayerEntry {
    props: LayerProps,
    /// 图像在画布上占的矩形，保证不出画布
    rect: Rect,
    file: Option<String>,
    kind: Kind,
    depth: u16,
    mask: Option<MaskEntry>,
    style: Option<Style>,
    artboard: Option<crate::document::Artboard>,
}

struct MaskEntry {
    rect: Rect,
    file: Option<String>,
    default: u16,
    disabled: bool,
}

/// 文件内容有问题时的错误。
fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// 读 `.zhibi` 文件。读出的文档没有撤销记录，也没有未保存的改动。
///
/// 文件打不开时原样返回系统的错误；文件内容不对（不是这种文件、损坏、版本太新……）时
/// 返回 `InvalidData`，附带说明。任何输入都不会让它崩溃。
pub fn load(path: &Path) -> io::Result<Document> {
    let file = File::open(path)?;
    let mut zip = ZipArchive::new(BufReader::new(file))
        .map_err(|e| invalid(trf!("不是纸笔文件（不是有效的 ZIP 压缩包）：{e}", "Not a Pen & Paper file (not a valid ZIP archive): {e}")))?;
    let header = parse_header(&read_document_json(&mut zip)?)?;

    let mut layers = Vec::with_capacity(header.layers.len());
    for (i, entry) in header.layers.into_iter().enumerate() {
        let mut tiles = TileMap::default();
        let n = i + 1;
        if let Some(name) = &entry.file {
            read_layer_png(&mut zip, name, entry.rect, &mut tiles).map_err(|e| {
                io::Error::new(e.kind(), trf!("第 {n} 个图层的图像 {name:.80}：{e}", "Image {name:.80} of layer {n}: {e}"))
            })?;
        }
        let mut layer = Layer::pixels(entry.props, tiles);
        layer.kind = entry.kind;
        layer.depth = entry.depth;
        layer.style = entry.style;
        layer.artboard = entry.artboard;
        if let Some(m) = entry.mask {
            let mut mask = Mask { tiles: GrayMap::default(), default: m.default, disabled: m.disabled };
            if let Some(name) = &m.file {
                read_mask_png(&mut zip, name, m.rect, &mut mask).map_err(|e| {
                    io::Error::new(e.kind(), trf!("第 {n} 个图层的蒙版 {name:.80}：{e}", "Mask {name:.80} of layer {n}: {e}"))
                })?;
            }
            layer.mask = Some(mask);
        }
        layers.push(layer);
    }
    let mut doc = Document::from_layers(header.width, header.height, header.background, layers);
    if let Some(dpi) = header.dpi {
        doc.dpi = dpi;
    }
    if let Some(active) = header.active {
        doc.set_active(active);
    }
    Ok(doc)
}

fn read_document_json<R: Read + Seek>(zip: &mut ZipArchive<R>) -> io::Result<Value> {
    let entry = match zip.by_name(DOCUMENT_JSON) {
        Ok(entry) => entry,
        Err(ZipError::FileNotFound) => {
            return Err(invalid(tr(
                "不是纸笔文件（压缩包里没有 document.json）",
                "Not a Pen & Paper file (the archive has no document.json)",
            )))
        }
        Err(e) => return Err(invalid(trf!("无法读取 document.json：{e}", "Could not read document.json: {e}"))),
    };
    // 压缩包里登记的大小不可信，按实际解出来的字节数设限。
    let mut text = Vec::new();
    entry
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut text)
        .map_err(|e| invalid(trf!("无法读取 document.json：{e}", "Could not read document.json: {e}")))?;
    if text.len() as u64 > MAX_JSON_BYTES {
        return Err(invalid(tr(
            "document.json 大得不正常，文件可能已损坏",
            "document.json is unusually large; the file may be damaged",
        )));
    }
    // 有的编辑器和脚本会在开头加上 UTF-8 的 BOM。
    let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&text);
    serde_json::from_slice(text)
        .map_err(|e| invalid(trf!("document.json 不是合法的 JSON：{e}", "document.json is not valid JSON: {e}")))
}

/// 检查 `document.json` 并取出要用的字段。`dpi` 和 `active` 只是附带的信息，
/// 缺失或者不对就用默认值；其余字段有问题都算文件坏了。
fn parse_header(root: &Value) -> io::Result<Header> {
    if root.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(invalid(tr(
            "不是纸笔文件（document.json 里的 format 不是 \"zhibi\"）",
            "Not a Pen & Paper file (format in document.json is not \"zhibi\")",
        )));
    }
    match root.get("version").and_then(Value::as_u64) {
        Some(1..=VERSION) => {}
        Some(v) if v > VERSION => {
            return Err(invalid(trf!(
                "这个文件是用更新版本的纸笔保存的（格式版本 {v}），请升级后再打开",
                "This file was saved by a newer version of Pen & Paper (format version {v}); update the program to open it"
            )));
        }
        _ => return Err(invalid(tr("document.json 里的 version 无效", "Invalid version in document.json"))),
    }

    let side = |key: &str| root.get(key).and_then(Value::as_i64);
    let (Some(width), Some(height)) = (side("width"), side("height")) else {
        return Err(invalid(tr(
            "document.json 里的 width、height 缺失或不是整数",
            "width or height in document.json is missing or not an integer",
        )));
    };
    let sides = 1..=MAX_SIDE as i64;
    if !sides.contains(&width) || !sides.contains(&height) {
        return Err(invalid(trf!(
            "画布尺寸无效：{width}×{height}（每边应在 1 到 {MAX_SIDE} 之间）",
            "Invalid canvas size: {width}×{height} (each side must be 1 to {MAX_SIDE})"
        )));
    }
    let (width, height) = (width as i32, height as i32);

    let background = root
        .get("background")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 3)
        .and_then(|a| {
            let mut rgb = [0u8; 3];
            for (out, v) in rgb.iter_mut().zip(a) {
                *out = u8::try_from(v.as_u64()?).ok()?;
            }
            Some(rgb)
        })
        .ok_or_else(|| {
            invalid(tr(
                "document.json 里的 background 应为三个 0 到 255 的整数",
                "background in document.json must be three integers from 0 to 255",
            ))
        })?;

    // 图层、蒙版的图像可以伸出画布，但不超过图层能放到的范围（实现方案 §3.15）。
    let keep = Document::keep_area(width, height);
    let layers = root
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(tr("document.json 里没有图层列表 layers", "document.json has no layer list (layers)")))?
        .iter()
        .enumerate()
        .map(|(i, v)| parse_layer(v, i, keep))
        .collect::<io::Result<Vec<_>>>()?;

    Ok(Header {
        width,
        height,
        dpi: root.get("dpi").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()),
        background,
        active: root.get("active").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok()),
        layers,
    })
}

fn parse_layer(v: &Value, index: usize, keep: Rect) -> io::Result<LayerEntry> {
    // 中文是"第 N 个图层"后面直接接上 `what`，英文是 "Layer N" 加空格再接。
    let n = index + 1;
    let bad = |what: &str| invalid(trf!("第 {n} 个图层{what}", "Layer {n} {what}"));
    let name = v.get("name").and_then(Value::as_str).ok_or_else(|| bad(tr("缺少名称 name", "has no name (name)")))?;
    let opacity = v
        .get("opacity")
        .and_then(Value::as_f64)
        .ok_or_else(|| bad(tr("缺少不透明度 opacity", "has no opacity (opacity)")))?;
    let visible = v
        .get("visible")
        .and_then(Value::as_bool)
        .ok_or_else(|| bad(tr("缺少是否可见 visible", "does not say whether it is visible (visible)")))?;
    let blend_id =
        v.get("blend").and_then(Value::as_str).ok_or_else(|| bad(tr("缺少混合模式 blend", "has no blend mode (blend)")))?;
    let blend = BlendMode::from_id(blend_id).ok_or_else(|| {
        bad(&trf!("的混合模式无法识别：{blend_id:.40}", "has an unknown blend mode: {blend_id:.40}"))
    })?;

    let (rect, file) = image_ref(v, keep).map_err(|what| bad(&what))?;

    // 版本 2 加的：种类、深度、文件夹、填充、蒙版。版本 1 的文件里没有，都是最外层的像素图层。
    let kind = match v.get("kind").and_then(Value::as_str) {
        None | Some("pixels") => Kind::Pixels,
        Some("folder") => Kind::Folder { open: v.get("open").and_then(Value::as_bool).unwrap_or(true) },
        Some("fill") => {
            let fill = v.get("fill").ok_or_else(|| bad(tr("是填充图层，却没有 fill", "is a fill layer without fill")))?;
            let paint = match fill.get("type").and_then(Value::as_str) {
                Some("solid") => {
                    let color = fill.get("color").and_then(Value::as_str).and_then(parse_color).ok_or_else(|| {
                        bad(tr("的填充颜色应为 #RRGGBB", "has a fill color that is not #RRGGBB"))
                    })?;
                    Paint::Solid(color)
                }
                Some("other") => Paint::Other,
                _ => return Err(bad(tr("的填充种类无法识别", "has an unknown fill type"))),
            };
            let psd = v.get("psd").and_then(|p| {
                let key: [u8; 4] = p.get("key")?.as_str()?.as_bytes().try_into().ok()?;
                Some((key, unbase64(p.get("data")?.as_str()?)?))
            });
            Kind::Fill(Fill { paint, psd })
        }
        Some(other) => return Err(bad(&trf!("的种类无法识别：{other:.40}", "has an unknown kind: {other:.40}"))),
    };
    let depth = v.get("depth").and_then(Value::as_u64).map_or(0, |d| d.min(u16::MAX as u64) as u16);
    let mask = match v.get("mask") {
        None | Some(Value::Null) => None,
        Some(m) => {
            let (rect, file) = image_ref(m, keep).map_err(|what| bad(&trf!("的蒙版{what}", "mask {what}")))?;
            let default = match m.get("default").and_then(Value::as_str) {
                Some("black") => 0,
                _ => ONE as u16,
            };
            Some(MaskEntry { rect, file, default, disabled: m.get("disabled").and_then(Value::as_bool).unwrap_or(false) })
        }
    };

    // 这几个是后来加的，旧文件里没有，缺了就当没开。
    let flag = |key: &str| v.get(key).and_then(Value::as_bool).unwrap_or(false);
    let props = LayerProps {
        name: name.into(),
        opacity: (opacity as f32).clamp(0.0, 1.0),
        visible,
        blend,
        lock_alpha: flag("lock_alpha"),
        locked: flag("locked"),
        clip: flag("clip"),
        pass_through: flag("pass_through"),
        fill: v.get("fill_opacity").and_then(Value::as_f64).map_or(1.0, |f| (f as f32).clamp(0.0, 1.0)),
        shapes: v.get("shapes").and_then(Value::as_bool).unwrap_or(true),
    };
    // 图层样式：原始的块照 PSD 的样子重新解析；读不懂的块照样留着。
    let style = v.get("style").filter(|s| !s.is_null()).map(|s| {
        let blocks: Vec<([u8; 4], Vec<u8>)> = s
            .get("psd")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|b| Some((b.get("key")?.as_str()?.as_bytes().try_into().ok()?, unbase64(b.get("data")?.as_str()?)?)))
                    .collect()
            })
            .unwrap_or_default();
        let light = s.get("light").and_then(Value::as_array).and_then(|a| Some((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32)));
        let refs: Vec<([u8; 4], &[u8])> = blocks.iter().map(|(k, d)| (*k, d.as_slice())).collect();
        let (mut style, _) = crate::psd::parse_style(&refs, light);
        style.enabled = s.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        style
    });
    let blocks = |v: &Value| -> Vec<([u8; 4], Vec<u8>)> {
        v.get("psd")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|b| Some((b.get("key")?.as_str()?.as_bytes().try_into().ok()?, unbase64(b.get("data")?.as_str()?)?)))
                    .collect()
            })
            .unwrap_or_default()
    };
    let artboard = v.get("artboard").filter(|a| !a.is_null()).map(|a| {
        let raw = blocks(a);
        let refs: Vec<([u8; 4], &[u8])> = raw.iter().map(|(k, d)| (*k, d.as_slice())).collect();
        crate::psd::parse_artboard(&refs)
    });
    Ok(LayerEntry { props, rect, file, kind, depth, mask, style, artboard })
}

/// `#RRGGBB` 写的颜色。
fn parse_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.strip_prefix('#').filter(|h| h.len() == 6)?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// 图层或蒙版的图像在画布上占的矩形和图像文件名（没有图像时是 None）。矩形要在 `keep`（图层能放到的范围）里。
/// 有问题时返回说明。
fn image_ref(v: &Value, keep: Rect) -> Result<(Rect, Option<String>), String> {
    let int = |key: &str| v.get(key).and_then(Value::as_i64);
    let (Some(x), Some(y), Some(w), Some(h)) = (int("x"), int("y"), int("width"), int("height")) else {
        return Err(tr("的矩形 x、y、width、height 缺失或不是整数", "has a missing or non-integer x, y, width or height").into());
    };
    let (kx0, ky0, kx1, ky1) = (keep.x0 as i64, keep.y0 as i64, keep.x1 as i64, keep.y1 as i64);
    // 先确认 x、y 在范围里再做减法，文件里写个天文数字也不会溢出。
    if !(kx0..=kx1).contains(&x) || !(ky0..=ky1).contains(&y) || !(0..=kx1 - x).contains(&w) || !(0..=ky1 - y).contains(&h) {
        return Err(trf!(
            "的矩形（x {x}、y {y}、宽 {w}、高 {h}）离画布太远",
            "has a rectangle (x {x}, y {y}, width {w}, height {h}) too far outside the canvas"
        ));
    }
    let file = match v.get("file") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(tr("的 file 应为文件名或 null", "has a file value that is not a file name or null").into()),
    };
    if file.is_some() && (w == 0 || h == 0) {
        return Err(tr("指定了图像，矩形却是空的", "names an image but its rectangle is empty").into());
    }
    Ok((Rect { x0: x as i32, y0: y as i32, x1: (x + w) as i32, y1: (y + h) as i32 }, file))
}

/// 把压缩包里的一张蒙版图像（16 位灰度）解码进蒙版。`rect` 非空且在画布之内。
fn read_mask_png<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str, rect: Rect, mask: &mut Mask) -> io::Result<()> {
    let (w, h) = ((rect.x1 - rect.x0) as u32, (rect.y1 - rect.y0) as u32);
    let entry = match zip.by_name(name) {
        Ok(entry) => entry,
        Err(ZipError::FileNotFound) => return Err(invalid(tr("不在压缩包里", "not in the archive"))),
        Err(e) => return Err(invalid(trf!("无法读取：{e}", "could not read: {e}"))),
    };
    let limit = 2 * 2 * w as u64 * h as u64 + (16 << 20);
    let mut decoder = png::Decoder::new(entry.take(limit));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let decode_error = |e: png::DecodingError| invalid(trf!("无法解码：{e}", "could not decode: {e}"));
    let info = decoder.read_header_info().map_err(decode_error)?;
    if (info.width, info.height) != (w, h) {
        return Err(invalid(tr("尺寸与 document.json 不一致", "size does not match document.json")));
    }
    if (info.color_type, info.bit_depth, info.interlaced) != (png::ColorType::Grayscale, png::BitDepth::Sixteen, false) {
        return Err(invalid(tr("不是 16 位灰度、不隔行的 PNG", "not a 16-bit grayscale, non-interlaced PNG")));
    }
    let mut reader = decoder.read_info().map_err(decode_error)?;
    let fill = mask.default;
    for y in rect.y0..rect.y1 {
        let row = reader.next_row().map_err(decode_error)?.ok_or_else(|| invalid(tr("图像数据不完整", "image data is incomplete")))?;
        if row.data().len() != w as usize * 2 {
            return Err(invalid(tr("图像数据与文件头不符", "image data does not match its header")));
        }
        for (k, b) in row.data().as_chunks::<2>().0.iter().enumerate() {
            let x = rect.x0 + k as i32;
            let (tx, ty) = (x.div_euclid(TILE), y.div_euclid(TILE));
            let v = div_round(u16::from_be_bytes(*b) as u64 * ONE64, MAX16) as u16;
            mask.tiles.get_mut((tx, ty), fill)[((y - ty * TILE) * TILE + (x - tx * TILE)) as usize] = v;
        }
    }
    reader.next_row().map_err(decode_error)?;
    Ok(())
}

/// 把压缩包里的一张图层图像解码进 `tiles`。`rect` 是 `document.json` 给它定的位置，非空且在画布之内。
fn read_layer_png<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str, rect: Rect, tiles: &mut TileMap) -> io::Result<()> {
    let (w, h) = ((rect.x1 - rect.x0) as u32, (rect.y1 - rect.y0) as u32);
    let stride = w as usize * 8;
    let entry = match zip.by_name(name) {
        Ok(entry) => entry,
        Err(ZipError::FileNotFound) => return Err(invalid(tr("不在压缩包里", "not in the archive"))),
        Err(e) => return Err(invalid(trf!("无法读取：{e}", "could not read: {e}"))),
    };
    // 正常的图层图像不会比没压缩的像素大出多少。给读取量设个上限，
    // 免得一个很小的压缩包解出没完没了的数据。
    let limit = 2 * stride as u64 * h as u64 + (16 << 20);
    let mut decoder = png::Decoder::new(entry.take(limit));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let decode_error = |e: png::DecodingError| invalid(trf!("无法解码：{e}", "could not decode: {e}"));

    // 图像自称的尺寸必须和 document.json 一致。刚读完文件头就查，确认之后才按这个尺寸分配内存、写分块。
    let info = decoder.read_header_info().map_err(decode_error)?;
    let (iw, ih) = (info.width, info.height);
    if (iw, ih) != (w, h) {
        return Err(invalid(trf!(
            "尺寸与 document.json 不一致（应为 {w}×{h}，实际是 {iw}×{ih}）",
            "size does not match document.json (should be {w}×{h}, is {iw}×{ih})"
        )));
    }
    if (info.color_type, info.bit_depth) != (png::ColorType::Rgba, png::BitDepth::Sixteen) {
        return Err(invalid(tr("不是 16 位 RGBA 的 PNG", "not a 16-bit RGBA PNG")));
    }
    let interlaced = info.interlaced;
    let mut reader = decoder.read_info().map_err(decode_error)?;

    if interlaced {
        // 隔行扫描的图只能整幅解码。本程序不会写出这种文件，别的程序写的可能是。
        let size = reader.output_buffer_size();
        let mut buf = Vec::new();
        buf.try_reserve_exact(size)
            .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, tr("内存不足", "out of memory")))?;
        buf.resize(size, 0u8);
        let frame = reader.next_frame(&mut buf).map_err(decode_error)?;
        if (frame.width, frame.height, frame.line_size) != (w, h, stride) {
            return Err(invalid(tr("图像数据与文件头不符", "image data does not match its header")));
        }
        for (y, row) in (rect.y0..rect.y1).zip(buf.chunks_exact(stride)) {
            store_row(tiles, rect.x0, y, row);
        }
    } else {
        for y in rect.y0..rect.y1 {
            let row = reader
                .next_row()
                .map_err(decode_error)?
                .ok_or_else(|| invalid(tr("图像数据不完整", "image data is incomplete")))?;
            if row.data().len() != stride {
                return Err(invalid(tr("图像数据与文件头不符", "image data does not match its header")));
            }
            store_row(tiles, rect.x0, y, row.data());
        }
        // 再要一行，让解码器把剩下的图像数据读完：数据块的校验和在块的末尾，读到那里才会核对。
        reader.next_row().map_err(decode_error)?;
    }
    Ok(())
}

/// 把一行 16 位 RGBA（大端、非预乘）写进分块，从画布坐标 `(x_start, y)` 开始往右（可以在画布外）。
/// 整段全透明的地方不为它新建分块，图层照旧是稀疏的。
fn store_row(tiles: &mut TileMap, x_start: i32, y: i32, row: &[u8]) {
    let x_end = x_start + (row.len() / 8) as i32;
    let ty = y.div_euclid(TILE);
    let mut x = x_start;
    while x < x_end {
        let tx = x.div_euclid(TILE);
        let x1 = ((tx + 1) * TILE).min(x_end);
        let (src, _) = row[((x - x_start) * 8) as usize..((x1 - x_start) * 8) as usize].as_chunks::<8>();
        if src.iter().any(|p| p[6] != 0 || p[7] != 0) {
            let tile = tiles.get_mut((tx, ty));
            let at = (((y - ty * TILE) * TILE + (x - tx * TILE)) * 4) as usize;
            for (p, out) in src.iter().zip(tile.0[at..].as_chunks_mut::<4>().0) {
                let be = |i: usize| u16::from_be_bytes([p[i], p[i + 1]]);
                *out = to_premultiplied([be(0), be(2), be(4), be(6)]);
            }
        }
        x = x1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的临时文件或目录，离开作用域时删掉（断言失败时也一样）。
    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let name = format!("zhibi-format-test-{}-{}-{tag}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            Temp(std::env::temp_dir().join(name))
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            if self.0.is_dir() {
                let _ = fs::remove_dir_all(&self.0);
            } else {
                let _ = fs::remove_file(&self.0);
            }
        }
    }

    /// 可重复的小随机数发生器（xorshift）。
    struct Rng(u32);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0
        }

        /// 一个合法的预乘像素，偏向容易出错的值：全透明、透明度极低、不透明、颜色等于透明度。
        fn pixel(&mut self) -> [u16; 4] {
            let a = match self.next() % 8 {
                0 => 0,
                1 => 1,
                2 => ONE,
                3 => self.next() % 16,
                _ => self.next() % (ONE + 1),
            };
            let mut c = || match self.next() % 4 {
                0 => 0,
                1 => a as u16,
                _ => (self.next() % (a + 1)) as u16,
            };
            [c(), c(), c(), a as u16]
        }
    }

    fn pixel(layer: &Layer, x: i32, y: i32) -> [u16; 4] {
        let (tx, ty) = (x.div_euclid(TILE), y.div_euclid(TILE));
        layer.tiles.get((tx, ty)).map_or([0; 4], |t| {
            let i = ((y - ty * TILE) * TILE + (x - tx * TILE)) as usize * 4;
            [t.0[i], t.0[i + 1], t.0[i + 2], t.0[i + 3]]
        })
    }

    fn set_pixel(layer: &mut Layer, x: i32, y: i32, px: [u16; 4]) {
        let (tx, ty) = (x.div_euclid(TILE), y.div_euclid(TILE));
        let i = ((y - ty * TILE) * TILE + (x - tx * TILE)) as usize * 4;
        layer.tiles.get_mut((tx, ty)).0[i..i + 4].copy_from_slice(&px);
    }

    fn read_entry(path: &Path, name: &str) -> Vec<u8> {
        let mut zip = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut data = Vec::new();
        zip.by_name(name).unwrap().read_to_end(&mut data).unwrap();
        data
    }

    fn decode_png(data: &[u8]) -> (png::OutputInfo, Vec<u8>) {
        let mut reader = png::Decoder::new(data).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info, buf)
    }

    fn encode_png(w: u32, h: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    /// 16 位 RGBA 的 PNG，所有像素都是 `px`（非预乘）。
    fn png16(w: u32, h: u32, px: [u16; 4]) -> Vec<u8> {
        let one: Vec<u8> = px.iter().flat_map(|v| v.to_be_bytes()).collect();
        encode_png(w, h, png::ColorType::Rgba, png::BitDepth::Sixteen, &one.repeat((w * h) as usize))
    }

    /// 用 zip 库直接打一个压缩包（条目用 deflate 压缩，和本程序自己写的不一样，顺便测到解压）。
    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            zip.start_file(*name, SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    /// 三个图层、尺寸不是 64 的倍数的文档，各种麻烦的情况都放了一点。
    fn sample_document() -> Document {
        let (w, h) = (200, 130);
        let mut rng = Rng(0x2545_F491);

        // 底层：铺满整张画布，全透明、半透明、不透明的像素都有。
        let mut paint = Layer::new("底色 paint");
        for y in 0..h {
            for x in 0..w {
                set_pixel(&mut paint, x, y, rng.pixel());
            }
        }
        // 四个角一定有内容，包围盒就是整张画布。
        set_pixel(&mut paint, 0, 0, [1, 0, 1, 1]);
        set_pixel(&mut paint, w - 1, 0, [ONE as u16; 4]);
        set_pixel(&mut paint, 0, h - 1, [3, 2, 1, 5]);
        set_pixel(&mut paint, w - 1, h - 1, [0, 0, 0, 1]);
        // 右边和下边的分块伸出画布，外面那部分也有内容：伸出画布的内容照样存（实现方案 §3.15）。
        for y in 0..192 {
            for x in 0..256 {
                if x >= w || y >= h {
                    set_pixel(&mut paint, x, y, [20000, 12345, 1, 30000]);
                }
            }
        }
        // 还有整个在画布外的分块：左边那块在图层能放到的范围里，右下那块离画布太远。
        paint.tiles.get_mut((7, 7)).0.fill(4000);
        paint.tiles.get_mut((-1, 0)).0.fill(123);

        // 中间：空图层。有一个全透明的分块和一个画布外的分块，仍然算空。
        let mut empty = Layer::new("空图层");
        empty.props = LayerProps { opacity: 0.37, visible: false, blend: BlendMode::Multiply, locked: true, ..empty.props };
        empty.tiles.get_mut((1, 1));
        empty.tiles.get_mut((9, 0)).0.fill(ONE as u16);

        // 顶层：离原点有一段距离的一小块，跨了好几个分块。
        let mut lines = Layer::new("线稿 ✏️");
        lines.props = LayerProps { opacity: 0.8125, blend: BlendMode::Overlay, lock_alpha: true, clip: true, ..lines.props };
        for y in 40..101 {
            for x in 70..91 {
                let a = (1 + (x * 331 + y * 577) % ONE as i32) as u16;
                set_pixel(&mut lines, x, y, [a / 2, a, 0, a]);
            }
        }

        let mut doc = Document::from_layers(w, h, [12, 200, 77], vec![paint, empty, lines]);
        doc.dpi = 144;
        doc.set_active(1);
        doc
    }

    #[test]
    fn round_trip_is_exact() {
        let doc = sample_document();
        let tmp = Temp::new("roundtrip.zhibi");
        save(&doc, &tmp.0).unwrap();
        let back = load(&tmp.0).unwrap();

        assert_eq!((back.width, back.height, back.dpi, back.background), (200, 130, 144, [12, 200, 77]));
        assert_eq!(back.active(), 1);
        assert!(!back.is_modified() && !back.can_undo() && !back.can_redo());
        assert_eq!(back.layers().len(), 3);
        for (a, b) in doc.layers().iter().zip(back.layers()) {
            assert_eq!(a.props, b.props);
            for y in 0..doc.height {
                for x in 0..doc.width {
                    assert_eq!(pixel(a, x, y), pixel(b, x, y), "图层“{}”的像素 ({x}, {y})", a.props.name);
                }
            }
        }
        assert_eq!(back.flatten_rgb(), doc.flatten_rgb());
        // 伸出画布的内容跟过来了，离画布太远的没有；空图层读回来一个分块也没有。
        for (x, y) in [(200, 0), (255, 191), (0, 130), (-64, 0), (-1, 63)] {
            assert_eq!(pixel(&back.layers()[0], x, y), pixel(&doc.layers()[0], x, y), "({x}, {y})");
        }
        assert!(back.layers()[0].tiles.get((7, 7)).is_none());
        assert!(back.layers()[1].tiles.is_empty());

        // document.json 里的矩形是内容的包围盒（伸出画布的也算）。
        let meta: Value = serde_json::from_slice(&read_entry(&tmp.0, DOCUMENT_JSON)).unwrap();
        assert_eq!((&meta["format"], &meta["version"], &meta["active"]), (&json!("zhibi"), &json!(2), &json!(1)));
        let rect = |i: usize| {
            let l = &meta["layers"][i];
            (l["x"].as_i64().unwrap(), l["y"].as_i64().unwrap(), l["width"].as_i64().unwrap(), l["height"].as_i64().unwrap())
        };
        assert_eq!(rect(0), (-64, 0, 320, 192));
        assert_eq!(rect(1), (0, 0, 0, 0));
        assert_eq!(rect(2), (70, 40, 21, 61));
        assert_eq!(meta["layers"][0]["file"], json!("layers/0000.png"));
        assert_eq!(meta["layers"][1]["file"], Value::Null);
        assert_eq!(meta["layers"][2]["file"], json!("layers/0002.png"));
        assert_eq!(meta["layers"][1]["opacity"].to_string(), "0.37");
        assert_eq!(meta["layers"][2]["blend"], json!("overlay"));
    }

    /// 文件里的图层图像确实是规定的样子：16 位、非预乘，只有内容所在的那块。
    #[test]
    fn layer_png_holds_straight_alpha_in_the_content_rectangle() {
        let mut layer = Layer::new("a");
        // 半透明：预乘的 (0.25, 0, 0.5, 0.5) 就是非预乘的 (0.5, 0, 1, 0.5)。
        set_pixel(&mut layer, 70, 3, [8192, 0, 16384, 16384]);
        set_pixel(&mut layer, 72, 4, [ONE as u16, 0, 0, ONE as u16]);
        let doc = Document::from_layers(100, 50, [255, 255, 255], vec![layer]);
        let tmp = Temp::new("straight.zhibi");
        save(&doc, &tmp.0).unwrap();

        let (info, data) = decode_png(&read_entry(&tmp.0, "layers/0000.png"));
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!((info.color_type, info.bit_depth), (png::ColorType::Rgba, png::BitDepth::Sixteen));
        let px = |x: usize, y: usize| -> Vec<u16> {
            data[(y * 3 + x) * 8..][..8].as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b)).collect()
        };
        assert_eq!(px(0, 0), [32768, 0, 65535, 32768]);
        assert_eq!(px(1, 0), [0, 0, 0, 0]);
        assert_eq!(px(2, 1), [65535, 0, 0, 65535]);
    }

    #[test]
    fn pixel_conversion_is_exactly_invertible() {
        let check = |px: [u16; 4]| assert_eq!(to_premultiplied(to_straight(px)), px, "{px:?} -> {:?}", to_straight(px));
        // 透明度：全部的值。颜色取 0、一半和等于透明度。
        for a in 0..=ONE as u16 {
            check([0, a / 2, a, a]);
        }
        for a in 1..=ONE as u16 {
            // 透明度很低、接近不透明、恰好是 2 的幂及其两侧：颜色取遍所有值。
            let dense = a <= 600 || a >= ONE as u16 - 600 || a.is_power_of_two() || (a + 1).is_power_of_two() || (a - 1).is_power_of_two();
            if dense {
                for c in 0..=a {
                    check([c, a - c, c, a]);
                }
            } else {
                // 其余的取两端、中点附近，再加一组稀疏的抽样。
                for c in [0, 1, 2, a / 3, a / 2 - 1, a / 2, a / 2 + 1, a - 2, a - 1, a] {
                    check([c, a - c, c, a]);
                }
                for c in (0..=a).step_by(251) {
                    check([c, a - c, a, a]);
                }
            }
        }
        // 两头的值落在 16 位的两头。
        assert_eq!(to_straight([ONE as u16; 4]), [65535; 4]);
        assert_eq!(to_straight([1, 0, 1, 1]), [65535, 0, 65535, 2]);
        assert_eq!(to_straight([5, 6, 7, 0]), [0; 4]);
        // 不合法的预乘值（颜色大于透明度、透明度大于 1）夹到范围内，不溢出。
        assert_eq!(to_straight([40000, 5, 70, 3]), [65535, 65535, 65535, 6]);
        assert_eq!(to_straight([65535; 4]), [65535; 4]);
        // 别的程序写的文件里，全透明像素可以带着颜色；读进来一律是全零。
        assert_eq!(to_premultiplied([65535, 1234, 0, 0]), [0; 4]);
        assert_eq!(to_premultiplied([65535; 4]), [ONE as u16; 4]);
    }

    /// 所有合法的（透明度，颜色）组合，五亿多个，要跑好几秒，平时不跑：
    /// `cargo test -p zhibi-core -- --ignored`
    #[test]
    #[ignore]
    fn pixel_conversion_is_exactly_invertible_for_every_value() {
        for a in 0..=ONE as u16 {
            for c in 0..=a {
                let px = [c, a - c, c, a];
                assert_eq!(to_premultiplied(to_straight(px)), px);
            }
        }
    }

    /// 不透明度写成最短的小数，经过 JSON 文本再读回来还是原来的 f32。
    #[test]
    fn opacity_survives_the_trip_through_json() {
        let mut rng = Rng(0x9E37_79B9);
        let mut values = vec![0.0, 1.0, 0.1, 0.3, 0.37, 1.0 / 3.0, 0.99999994, 1e-10, f32::MIN_POSITIVE, 1e-45];
        values.extend((0..=1000).map(|k| k as f32 / 1000.0));
        values.extend((0..100_000).map(|_| rng.next() as f32 / u32::MAX as f32));
        for v in values {
            let text = json!(opacity_number(v)).to_string();
            let back: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(back.as_f64().unwrap() as f32, v, "{v} 写成了 {text}");
        }
        assert_eq!(json!(opacity_number(0.3)).to_string(), "0.3");
        // 范围外的值夹回 0 到 1。
        assert_eq!((opacity_number(-2.0), opacity_number(7.5), opacity_number(f32::NAN)), (0.0, 1.0, 0.0));
    }

    #[test]
    fn merged_and_thumbnail_previews() {
        // 左半边是不透明的红色，右半边露出背景。
        let mut left = Layer::new("左半");
        for y in 0..300 {
            for x in 0..300 {
                set_pixel(&mut left, x, y, [ONE as u16, 0, 0, ONE as u16]);
            }
        }
        let doc = Document::from_layers(600, 300, [10, 20, 30], vec![left]);
        let tmp = Temp::new("preview.zhibi");
        save(&doc, &tmp.0).unwrap();

        let (info, merged) = decode_png(&read_entry(&tmp.0, MERGED_PNG));
        assert_eq!((info.width, info.height), (600, 300));
        assert_eq!((info.color_type, info.bit_depth), (png::ColorType::Rgb, png::BitDepth::Eight));
        assert_eq!(merged, doc.flatten_rgb());

        let (info, thumb) = decode_png(&read_entry(&tmp.0, THUMBNAIL_PNG));
        assert_eq!((info.width, info.height), (256, 128));
        assert_eq!((info.color_type, info.bit_depth), (png::ColorType::Rgb, png::BitDepth::Eight));
        let at = |x: usize, y: usize| &thumb[(y * 256 + x) * 3..][..3];
        assert_eq!(at(0, 0), [255, 0, 0]);
        assert_eq!(at(127, 64), [255, 0, 0]);
        assert_eq!(at(128, 64), [10, 20, 30]);
        assert_eq!(at(255, 127), [10, 20, 30]);

        // 其他形状：小图不放大，细长的图短边至少留一个像素，求平均时四舍五入。
        assert_eq!(thumbnail(&[9; 100 * 50 * 3], 100, 50), (vec![9; 100 * 50 * 3], 100, 50));
        assert_eq!(thumbnail(&vec![7; 16384 * 3], 16384, 1), (vec![7; 256 * 3], 256, 1));
        let (_, tw, th) = thumbnail(&vec![0; 257 * 300 * 3], 257, 300);
        assert_eq!((tw, th), (219, 256));
        let stripes: Vec<u8> = (0..512 * 3).map(|i| if (i / 3) % 2 == 0 { 0 } else { 255 }).collect();
        assert_eq!(thumbnail(&stripes, 512, 1), (vec![128; 256 * 3], 256, 1));
    }

    #[test]
    fn saving_over_an_existing_file_replaces_it_and_leaves_no_temp_files() {
        let dir = Temp::new("dir");
        fs::create_dir(&dir.0).unwrap();
        let path = dir.0.join(format!("画.{EXTENSION}"));
        let listing = || {
            let mut names: Vec<_> = fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
            names.sort();
            names
        };

        save(&Document::new(64, 48), &path).unwrap();
        assert_eq!(load(&path).unwrap().width, 64);
        let second = sample_document();
        save(&second, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!((back.width, back.height, back.layers().len()), (200, 130, 3));
        assert_eq!(listing(), [path.file_name().unwrap()]);

        // 存不成的时候报错，目标原样不动，临时文件也收拾掉。这里的目标是个目录，改名顶替不了它。
        let blocked = dir.0.join("挡路的目录");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("里面有东西"), b"x").unwrap();
        assert!(save(&second, &blocked).is_err());
        assert!(blocked.join("里面有东西").is_file());
        assert_eq!(listing().len(), 2);
        // 目录不存在时连临时文件都建不出来。
        assert!(save(&second, &dir.0.join("没有这个目录").join("a.zhibi")).is_err());
        assert_eq!(listing().len(), 2);
    }

    // ---- 读别人写的文件、坏文件 ----

    /// 一份最小的合法 `document.json`：100×80 的画布，一个图层，图像是 (5, 6) 处 10×4 的一块。
    fn base_meta() -> Value {
        json!({
            "format": "zhibi",
            "version": 1,
            "width": 100,
            "height": 80,
            "dpi": 72,
            "background": [1, 2, 3],
            "active": 0,
            "layers": [{
                "name": "脚本写的图层",
                "opacity": 0.5,
                "visible": true,
                "blend": "screen",
                "x": 5,
                "y": 6,
                "width": 10,
                "height": 4,
                "file": "layers/0000.png",
            }],
        })
    }

    /// 配得上 `base_meta` 的图层图像：半透明的纯红。
    fn base_png() -> Vec<u8> {
        png16(10, 4, [65535, 0, 0, 32768])
    }

    /// 把 `document.json` 的文本和（可选的）图层图像打成压缩包再读。
    fn load_parts(meta: &str, layer_png: Option<&[u8]>) -> io::Result<Document> {
        let tmp = Temp::new("parts.zhibi");
        let mut entries = vec![(DOCUMENT_JSON, meta.as_bytes())];
        if let Some(png) = layer_png {
            entries.push(("layers/0000.png", png));
        }
        write_zip(&tmp.0, &entries);
        load(&tmp.0)
    }

    /// 在 `base_meta` 上改一处再读。
    fn load_changed(change: impl FnOnce(&mut Value)) -> io::Result<Document> {
        let mut meta = base_meta();
        change(&mut meta);
        load_parts(&meta.to_string(), Some(&base_png()))
    }

    /// 必须是 `InvalidData`，而且错误信息说的是预期的那件事。
    fn expect_invalid(result: io::Result<Document>, keyword: &str) {
        let e = result.err().expect("应当报错");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        assert!(e.to_string().contains(keyword), "错误信息里应当有“{keyword}”：{e}");
    }

    #[test]
    fn loads_a_file_written_by_another_tool() {
        // 先确认没改过的那份能读，下面各个坏文件的测试才说明问题。
        let doc = load_parts(&base_meta().to_string(), Some(&base_png())).unwrap();
        assert_eq!((doc.width, doc.height, doc.dpi, doc.background, doc.active()), (100, 80, 72, [1, 2, 3], 0));
        let layer = &doc.layers()[0];
        let props = LayerProps { name: "脚本写的图层".into(), opacity: 0.5, blend: BlendMode::Screen, ..Default::default() };
        assert_eq!(layer.props, props);
        assert_eq!(pixel(layer, 5, 6), [16384, 0, 0, 16384]);
        assert_eq!(pixel(layer, 14, 9), [16384, 0, 0, 16384]);
        assert_eq!(pixel(layer, 4, 6), [0; 4]);
        assert_eq!(pixel(layer, 15, 9), [0; 4]);
        assert_eq!(pixel(layer, 5, 10), [0; 4]);
        assert!(!doc.is_modified() && !doc.can_undo());

        // 不认识的字段忽略；dpi 和 active 缺失或不对时用默认值；开头的 BOM 不碍事。
        let doc = load_changed(|m| {
            m["made_by"] = json!({ "tool": "某个脚本", "version": [1, 2] });
            m["layers"][0]["some_future_field"] = json!(true);
            m["layers"][0]["clip"] = json!("yes");
            m["layers"][0]["opacity"] = json!(3);
            m["layers"].as_array_mut().unwrap().push(json!({
                "name": "空", "opacity": 1, "visible": false, "blend": "normal", "x": 0, "y": 0, "width": 0, "height": 0,
            }));
            m.as_object_mut().unwrap().remove("dpi");
            m["active"] = json!(99);
        })
        .unwrap();
        assert_eq!((doc.dpi, doc.active(), doc.layers().len()), (300, 1, 2));
        assert_eq!(doc.layers()[0].props.opacity, 1.0);
        assert!(doc.layers()[1].tiles.is_empty());
        let with_bom = format!("\u{FEFF}{}", base_meta());
        assert_eq!(load_parts(&with_bom, Some(&base_png())).unwrap().width, 100);
        // 没有图层的文件也能读，得到一个空图层。
        assert_eq!(load_changed(|m| m["layers"] = json!([])).unwrap().layers().len(), 1);
    }

    #[test]
    fn rejects_files_that_are_not_ours() {
        let tmp = Temp::new("garbage.zhibi");
        assert_eq!(load(&tmp.0).err().expect("文件不存在").kind(), io::ErrorKind::NotFound);
        fs::write(&tmp.0, b"").unwrap();
        expect_invalid(load(&tmp.0), "ZIP");
        fs::write(&tmp.0, "这只是一段文字，不是压缩包。".repeat(50)).unwrap();
        expect_invalid(load(&tmp.0), "ZIP");
        fs::write(&tmp.0, base_png()).unwrap();
        expect_invalid(load(&tmp.0), "ZIP");
        // 截断的压缩包。
        let good = Temp::new("good.zhibi");
        save(&sample_document(), &good.0).unwrap();
        let bytes = fs::read(&good.0).unwrap();
        fs::write(&tmp.0, &bytes[..bytes.len() / 2]).unwrap();
        expect_invalid(load(&tmp.0), "ZIP");

        // 是压缩包，但不是这种文件。
        write_zip(&tmp.0, &[("readme.txt", b"hello")]);
        expect_invalid(load(&tmp.0), "document.json");
        expect_invalid(load_parts("{ \"format\": \"zhibi\", ", None), "JSON");
        expect_invalid(load_parts("", None), "JSON");
        expect_invalid(load_parts("[1, 2, 3]", None), "format");
        expect_invalid(load_changed(|m| m["format"] = json!("openraster")), "format");
        expect_invalid(load_changed(|m| m["format"] = json!(1)), "format");
    }

    #[test]
    fn rejects_bad_versions_and_canvas_sizes() {
        expect_invalid(load_changed(|m| m["version"] = json!(3)), "更新版本");
        expect_invalid(load_changed(|m| m["version"] = json!(u64::MAX)), "更新版本");
        expect_invalid(load_changed(|m| m["version"] = json!(0)), "version");
        expect_invalid(load_changed(|m| m["version"] = json!("1")), "version");
        expect_invalid(load_changed(|m| m["version"] = json!(-1)), "version");

        expect_invalid(load_changed(|m| m["width"] = json!(0)), "画布尺寸");
        expect_invalid(load_changed(|m| m["height"] = json!(-5)), "画布尺寸");
        expect_invalid(load_changed(|m| m["width"] = json!(MAX_SIDE + 1)), "画布尺寸");
        expect_invalid(load_changed(|m| m["height"] = json!(i64::MAX)), "画布尺寸");
        expect_invalid(load_changed(|m| m["width"] = json!(100.5)), "width");
        expect_invalid(load_changed(|m| m["height"] = json!("80")), "height");
        expect_invalid(load_changed(|m| drop(m.as_object_mut().unwrap().remove("width"))), "width");
        // 最大的画布本身是合法的。
        assert_eq!(load_changed(|m| m["width"] = json!(MAX_SIDE)).unwrap().width, MAX_SIDE);

        expect_invalid(load_changed(|m| m["background"] = json!([1, 2])), "background");
        expect_invalid(load_changed(|m| m["background"] = json!([1, 2, 256])), "background");
        expect_invalid(load_changed(|m| m["layers"] = json!("none")), "layers");
    }

    #[test]
    fn rejects_bad_layer_entries() {
        let layer = |change: fn(&mut Value)| load_changed(|m| change(&mut m["layers"][0]));
        // 矩形伸出画布太远（画布 100×80，图层能放到 −100..200、−80..160）。
        expect_invalid(layer(|l| l["x"] = json!(191)), "离画布太远");
        expect_invalid(layer(|l| l["y"] = json!(157)), "离画布太远");
        expect_invalid(layer(|l| l["x"] = json!(-101)), "离画布太远");
        expect_invalid(layer(|l| l["width"] = json!(-10)), "离画布太远");
        expect_invalid(layer(|l| l["height"] = json!(i64::MAX)), "离画布太远");
        expect_invalid(layer(|l| l["x"] = json!(i64::MIN)), "离画布太远");
        expect_invalid(layer(|l| l["width"] = json!(1i64 << 40)), "离画布太远");
        expect_invalid(layer(|l| l["y"] = json!(1.5)), "不是整数");
        expect_invalid(layer(|l| l["width"] = json!(0)), "矩形却是空的");
        // 刚好贴着画布右下角、伸出画布、刚好贴着能放到的范围的右下角，都是可以的。
        for (x, y) in [(90, 76), (91, 77), (-100, -80), (190, 156)] {
            assert!(
                load_changed(|m| {
                    m["layers"][0]["x"] = json!(x);
                    m["layers"][0]["y"] = json!(y);
                })
                .is_ok(),
                "({x}, {y})"
            );
        }

        expect_invalid(layer(|l| l["blend"] = json!("dissolve")), "混合模式");
        expect_invalid(layer(|l| l["blend"] = json!(3)), "混合模式");
        expect_invalid(layer(|l| l["name"] = json!(null)), "name");
        expect_invalid(layer(|l| l["opacity"] = json!("half")), "opacity");
        expect_invalid(layer(|l| l["visible"] = json!(1)), "visible");
        expect_invalid(layer(|l| l["file"] = json!(7)), "file");
        expect_invalid(load_changed(|m| m["layers"][0] = json!("图层")), "第 1 个图层");
    }

    #[test]
    fn rejects_missing_or_mismatched_layer_images() {
        let meta = base_meta().to_string();
        let with_png = |png: &[u8]| load_parts(&meta, Some(png));
        // 图像不在压缩包里。
        expect_invalid(load_parts(&meta, None), "不在压缩包里");
        expect_invalid(load_changed(|m| m["layers"][0]["file"] = json!("layers/0001.png")), "不在压缩包里");
        // 尺寸不对。
        expect_invalid(with_png(&png16(11, 4, [0; 4])), "尺寸");
        expect_invalid(with_png(&png16(4, 10, [0; 4])), "尺寸");
        // 只有文件头、自称 16384×16384 的图：在分配内存之前就该被挡下。
        let mut huge = Vec::new();
        let mut enc = png::Encoder::new(&mut huge, 16384, 16384);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Sixteen);
        drop(enc.write_header().unwrap());
        expect_invalid(with_png(&huge), "尺寸");
        // 尺寸对得上、但文件头后面什么都没有的图。
        let mut hollow = Vec::new();
        let mut enc = png::Encoder::new(&mut hollow, 10, 4);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Sixteen);
        drop(enc.write_header().unwrap());
        expect_invalid(with_png(&hollow), "无法解码");
        // 类型不对。
        expect_invalid(with_png(&encode_png(10, 4, png::ColorType::Rgba, png::BitDepth::Eight, &[0; 160])), "16 位 RGBA");
        expect_invalid(with_png(&encode_png(10, 4, png::ColorType::Rgb, png::BitDepth::Sixteen, &[0; 240])), "16 位 RGBA");
        // 根本不是 PNG、截断的 PNG、被改坏的 PNG。
        expect_invalid(with_png(b"not a png at all"), "无法解码");
        expect_invalid(with_png(b""), "无法解码");
        let good = base_png();
        expect_invalid(with_png(&good[..good.len() - 20]), "图层的图像");
        let mut corrupt = good.clone();
        let middle = corrupt.len() / 2;
        corrupt[middle] ^= 0x55;
        expect_invalid(with_png(&corrupt), "图层的图像");
    }

    #[test]
    fn error_messages_in_english() {
        let message = |result: io::Result<Document>| result.err().expect("应当报错").to_string();
        let mut bad_size = Document::new(8, 8);
        bad_size.width = 0;
        crate::lang::set_english(true);
        let errors = [
            message(load_parts("", None)),
            message(load_changed(|m| m["format"] = json!("openraster"))),
            message(load_changed(|m| m["version"] = json!(3))),
            message(load_changed(|m| m["width"] = json!(0))),
            message(load_changed(|m| m["layers"][0]["x"] = json!(191))),
            message(load_changed(|m| m["layers"][0]["blend"] = json!("dissolve"))),
            message(load_parts(&base_meta().to_string(), None)),
            message(load_parts(&base_meta().to_string(), Some(&png16(11, 4, [0; 4])))),
            save(&bad_size, Path::new("never-written.zhibi")).err().unwrap().to_string(),
        ];
        crate::lang::set_english(false);
        assert!(errors[0].starts_with("document.json is not valid JSON: "), "{}", errors[0]);
        assert_eq!(errors[3], format!("Invalid canvas size: 0×80 (each side must be 1 to {MAX_SIDE})"));
        assert_eq!(
            errors[1..3],
            [
                r#"Not a Pen & Paper file (format in document.json is not "zhibi")"#,
                "This file was saved by a newer version of Pen & Paper (format version 3); update the program to open it",
            ]
        );
        assert_eq!(
            errors[4..],
            [
                "Layer 1 has a rectangle (x 191, y 6, width 10, height 4) too far outside the canvas",
                "Layer 1 has an unknown blend mode: dissolve",
                "Image layers/0000.png of layer 1: not in the archive",
                "Image layers/0000.png of layer 1: size does not match document.json (should be 10×4, is 11×4)",
                "Invalid canvas size: 0×8",
            ]
        );
        // 换回中文之后还是原来的中文。
        expect_invalid(load_parts("", None), "document.json 不是合法的 JSON：");
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    /// 手工拼一张隔行扫描（Adam7）的 16 位 RGBA PNG——png 库的编码器写不出这种文件。
    /// zlib 那一层用不压缩的"存储块"，所以图像数据不能超过 65535 字节。
    fn interlaced_png(w: usize, h: usize, pixel: impl Fn(usize, usize) -> [u16; 4]) -> Vec<u8> {
        // 七遍扫描各自的起点和步长：(x0, y0, dx, dy)
        const PASSES: [(usize, usize, usize, usize); 7] =
            [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];
        let mut raw = Vec::new();
        for (x0, y0, dx, dy) in PASSES {
            if x0 >= w {
                continue;
            }
            for y in (y0..h).step_by(dy) {
                raw.push(0); // 这一行的滤波方式：无
                for x in (x0..w).step_by(dx) {
                    raw.extend(pixel(x, y).iter().flat_map(|v| v.to_be_bytes()));
                }
            }
        }
        let len = u16::try_from(raw.len()).unwrap();
        let mut idat = vec![0x78, 0x01, 0x01];
        idat.extend(len.to_le_bytes());
        idat.extend((!len).to_le_bytes());
        idat.extend(&raw);
        idat.extend(adler32(&raw).to_be_bytes());
        let mut ihdr = Vec::new();
        ihdr.extend((w as u32).to_be_bytes());
        ihdr.extend((h as u32).to_be_bytes());
        ihdr.extend([16, 6, 0, 0, 1]); // 位深 16、RGBA、压缩方式 0、滤波方式 0、隔行扫描

        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in [(b"IHDR", &ihdr), (b"IDAT", &idat), (b"IEND", &Vec::new())] {
            out.extend((data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend(kind);
            out.extend(data);
            out.extend(crc32(&out[start..]).to_be_bytes());
        }
        out
    }

    #[test]
    fn loads_an_interlaced_layer_image() {
        // 9×6 的图七遍扫描都用得上；放在 (60, 62)，横竖都跨过分块的边界。
        let source = |x: usize, y: usize| [(x * 7000) as u16, (y * 9000) as u16, 65535, (65535 - 3000 * (x + y)) as u16];
        let png = interlaced_png(9, 6, source);
        // 这张手工拼的图先要是一张合法的 PNG。
        let (info, data) = decode_png(&png);
        assert_eq!((info.width, info.height, data.len()), (9, 6, 9 * 6 * 8));

        let mut meta = base_meta();
        let layer = &mut meta["layers"][0];
        (layer["x"], layer["y"], layer["width"], layer["height"]) = (json!(60), json!(62), json!(9), json!(6));
        let doc = load_parts(&meta.to_string(), Some(&png)).unwrap();
        for y in 0..80 {
            for x in 0..100 {
                let inside = (60..69).contains(&x) && (62..68).contains(&y);
                let want = if inside { to_premultiplied(source(x as usize - 60, y as usize - 62)) } else { [0; 4] };
                assert_eq!(pixel(&doc.layers()[0], x, y), want, "({x}, {y})");
            }
        }
    }

    fn mask_value(layer: &Layer, x: i32, y: i32) -> Option<u16> {
        let m = layer.mask.as_ref()?;
        let (tx, ty) = (x.div_euclid(TILE), y.div_euclid(TILE));
        Some(m.tiles.get((tx, ty)).map_or(m.default, |t| t[((y - ty * TILE) * TILE + (x - tx * TILE)) as usize]))
    }

    #[test]
    fn round_trip_keeps_folders_masks_and_fills() {
        let mut rng = Rng(77);
        let mut inner = Layer::new("里面");
        for y in 10..40 {
            for x in 20..90 {
                set_pixel(&mut inner, x, y, rng.pixel());
            }
        }
        inner.depth = 1;
        // 默认全黑、停用着、里面的值各不相同的蒙版，跨着分块的边界。
        let mut mask = Mask { tiles: GrayMap::default(), default: 0, disabled: true };
        for y in 50..80 {
            for x in 50..140 {
                let v = (rng.next() % (ONE + 1)) as u16;
                mask.tiles.get_mut((x / TILE, y / TILE), 0)[((y % TILE) * TILE + x % TILE) as usize] = v;
            }
        }
        inner.mask = Some(mask);
        let mut folder = Layer::folder("文件夹");
        folder.kind = Kind::Folder { open: false };
        folder.props.blend = BlendMode::Multiply;
        folder.props.pass_through = false;
        folder.mask = Some(Mask::white());
        let mut solid = Layer::new("颜色填充 1");
        solid.kind = Kind::Fill(Fill { paint: Paint::Solid([12, 34, 56]), psd: Some((*b"SoCo", (0..=255).collect())) });
        let mut other = Layer::new("渐变填充 1");
        other.kind = Kind::Fill(Fill { paint: Paint::Other, psd: Some((*b"GdFl", vec![1, 2, 3, 4, 5])) });
        other.props.visible = false;
        let doc = Document::from_layers(160, 100, [255; 3], vec![inner, folder, solid, other]);

        let tmp = Temp::new("tree.zhibi");
        save(&doc, &tmp.0).unwrap();
        let back = load(&tmp.0).unwrap();
        assert_eq!(back.layers().len(), 4);
        for (a, b) in doc.layers().iter().zip(back.layers()) {
            assert_eq!((&a.props, &a.kind, a.depth), (&b.props, &b.kind, b.depth), "{}", a.props.name);
            assert_eq!(a.mask.as_ref().map(|m| (m.default, m.disabled)), b.mask.as_ref().map(|m| (m.default, m.disabled)));
            for y in 0..doc.height {
                for x in 0..doc.width {
                    assert_eq!(pixel(a, x, y), pixel(b, x, y), "{} 的像素 ({x}, {y})", a.props.name);
                    assert_eq!(mask_value(a, x, y), mask_value(b, x, y), "{} 的蒙版 ({x}, {y})", a.props.name);
                }
            }
        }
        assert_eq!(back.flatten_rgb(), doc.flatten_rgb());

        // 版本 1 的文件（没有这些字段）照常读，全部是最外层的像素图层。
        let doc = load_changed(|m| m["version"] = json!(1)).unwrap();
        assert!(doc.layers().iter().all(|l| l.is_pixels() && l.depth == 0 && l.mask.is_none()));
        // 不认识的种类、坏的填充颜色算文件坏了。
        expect_invalid(load_changed(|m| m["layers"][0]["kind"] = json!("text")), "种类");
        expect_invalid(
            load_changed(|m| {
                m["layers"][0]["kind"] = json!("fill");
                m["layers"][0]["fill"] = json!({ "type": "solid", "color": "red" });
            }),
            "填充颜色",
        );
    }

    #[test]
    fn base64_round_trips() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b""), "");
        for n in 0..300 {
            let data: Vec<u8> = (0..n).map(|k| (k * 37 + n) as u8).collect();
            assert_eq!(unbase64(&base64(&data)).as_deref(), Some(&data[..]));
        }
        assert_eq!(unbase64("TW!u"), None);
    }

    /// 给窗口测试用的文档（实现方案 0.3 的实测）：`ZHIBI_INTEROP_DIR` 指着的目录里写
    /// `tree.zhibi`（文件夹、蒙版、填充图层，同 PSD 的测试）、`many-flat.zhibi` 和 `many-folders.zhibi`
    /// （2000×1500，200 个半透明图层，各有一块 500×400；后一个分在 20 个文件夹里，一半穿透）。
    #[test]
    #[ignore]
    fn write_window_test_docs() {
        use crate::document::Layer;
        use crate::tile::TILE;
        let Some(dir) = std::env::var_os("ZHIBI_INTEROP_DIR").map(std::path::PathBuf::from) else { return };
        save(&crate::psd::tests::tree_document(), &dir.join("tree.zhibi")).unwrap();
        for folders in [false, true] {
            let mut layers = Vec::new();
            for g in 0..20 {
                for k in 0..10 {
                    let mut l = Layer::new(&format!("图层 {g}-{k}"));
                    l.depth = folders as u16;
                    l.props.opacity = 0.6;
                    let (x0, y0) = ((g % 5) * 300 + k * 20, (g / 5) * 280 + k * 15);
                    let c = [(g * 12) as u16 * 128, (k * 25) as u16 * 128, 90 * 128, crate::ONE as u16];
                    for y in y0..y0 + 400 {
                        for x in x0..x0 + 500 {
                            let t = l.tiles.get_mut((x / TILE, y / TILE));
                            let i = ((y % TILE) * TILE + x % TILE) as usize * 4;
                            t.0[i..i + 4].copy_from_slice(&c);
                        }
                    }
                    layers.push(l);
                }
                if folders {
                    let mut f = Layer::folder(&format!("文件夹 {g}"));
                    f.props.pass_through = g % 2 == 0;
                    layers.push(f);
                }
            }
            let doc = Document::from_layers(2000, 1500, [255; 3], layers);
            save(&doc, &dir.join(if folders { "many-folders.zhibi" } else { "many-flat.zhibi" })).unwrap();
        }
    }
}
