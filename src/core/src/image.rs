// SPDX-License-Identifier: GPL-3.0-or-later
//! 单张图片：PNG 和 JPEG 的读写（规格 F-143、F-147、F-148，实现方案 §3.19）。
//!
//! - 读：按文件开头认格式，不看扩展名。PNG 和 ORA 的图层图像走同一段解码（`ora::decode_png`）；JPEG 用
//!   `zune-jpeg` 解成 8 位 RGB（灰度、CMYK 由它转换），EXIF 里的拍摄方向照着转正。读出来是一张分块表，
//!   左上角在 (0, 0)。打开成文档时它是唯一的图层。
//! - 写：整张画布合成后的一张图。PNG 可以垫背景色（RGB）或者不垫（RGBA，"透明背景"）；JPEG 用
//!   `jpeg-encoder`，垫背景色，质量 92，色度不抽样。都写打印分辨率。
//! - 没有色彩管理：ICC 颜色配置、`gAMA` 不管，像素值照原样当 sRGB 用。

use std::fs::{self, File};
use std::io::{self, BufWriter, Cursor};
use std::path::Path;

use crate::document::{Document, Layer, MAX_SIDE};
use crate::lang::tr;
use crate::ora::shared::{invalid, save_atomically, store_row};
use crate::ora::{decode_png, png_pixels_per_meter, PngOutcome, PNG_SIGNATURE};
use crate::tile::{tiles_in, TileMap, TILE};
use crate::trf;
use crate::{Rect, ONE};

/// 能打开、导入的扩展名（不含点）。
pub const EXTENSIONS: [&str; 3] = ["png", "jpg", "jpeg"];

/// 文件里没写分辨率时按这个算（PS 打开这种图片也是 72）。
const DEFAULT_DPI: u32 = 72;
/// JPEG 的质量（1–100）。
const JPEG_QUALITY: u8 = 92;
const JPEG_SIGNATURE: &[u8] = &[0xFF, 0xD8, 0xFF];

/// 读进来的一张图。
pub struct Picture {
    pub width: i32,
    pub height: i32,
    /// 15 位、预乘的像素，左上角在 (0, 0)
    pub tiles: TileMap,
    /// 文件里记着的打印分辨率
    pub dpi: Option<u32>,
    /// 有不是完全不透明的像素
    pub transparent: bool,
}

impl Picture {
    pub fn bounds(&self) -> Rect {
        Rect { x0: 0, y0: 0, x1: self.width, y1: self.height }
    }
}

/// 读一张 PNG 或 JPEG。
pub fn read(path: &Path) -> io::Result<Picture> {
    decode(&fs::read(path)?)
}

/// 打开一张图片：画布和它一样大，背景白色，图片是唯一的图层。另外返回图里有没有透明的像素。
pub fn load(path: &Path) -> io::Result<(Document, bool)> {
    let picture = read(path)?;
    let mut layer = Layer::new(tr("图层 1", "Layer 1"));
    layer.tiles = picture.tiles;
    let mut doc = Document::from_layers(picture.width, picture.height, [255; 3], vec![layer]);
    doc.dpi = picture.dpi.unwrap_or(DEFAULT_DPI);
    Ok((doc, picture.transparent))
}

/// 按内容认格式，解码一张图。
pub fn decode(bytes: &[u8]) -> io::Result<Picture> {
    if bytes.starts_with(PNG_SIGNATURE) {
        decode_png_file(bytes)
    } else if bytes.starts_with(JPEG_SIGNATURE) {
        decode_jpeg(bytes)
    } else {
        Err(invalid(tr("不是 PNG 或 JPEG 图片", "not a PNG or JPEG image")))
    }
}

/// 图片的尺寸放不放得进画布。
fn check_size(w: u32, h: u32) -> io::Result<()> {
    if w == 0 || h == 0 {
        return Err(invalid(tr("图片的尺寸不对", "the image has an invalid size")));
    }
    if w > MAX_SIDE as u32 || h > MAX_SIDE as u32 {
        return Err(invalid(trf!("图片太大（{w}×{h}），边长最多 {MAX_SIDE}", "The image is too large ({w}×{h}); at most {MAX_SIDE} on a side")));
    }
    Ok(())
}

fn decode_png_file(bytes: &[u8]) -> io::Result<Picture> {
    // 宽和高在 IHDR 数据块里的固定位置上。先看尺寸再解码，免得一张很大的图先把内存吃光。
    let field = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(w), Some(h)) = (field(16), field(20)) else {
        return Err(invalid(tr("无法解码：文件头不完整", "could not decode: incomplete header")));
    };
    check_size(w, h)?;
    let area = Rect { x0: 0, y0: 0, x1: w as i32, y1: h as i32 };
    let mut tiles = TileMap::default();
    let dpi = match decode_png(Cursor::new(bytes), (0, 0), area, &mut tiles)? {
        PngOutcome::Read { dpi, .. } => dpi,
        PngOutcome::NotPng => return Err(invalid(tr("不是 PNG 或 JPEG 图片", "not a PNG or JPEG image"))),
    };
    let transparent = has_transparency(&tiles, area);
    Ok(Picture { width: area.x1, height: area.y1, tiles, dpi, transparent })
}

/// `area` 里有没有不是完全不透明的像素（没有分块的地方是透明的）。
fn has_transparency(tiles: &TileMap, area: Rect) -> bool {
    tiles_in(area).any(|tc| {
        let Some(tile) = tiles.get(tc) else { return true };
        let r = Rect { x0: tc.0 * TILE, y0: tc.1 * TILE, x1: (tc.0 + 1) * TILE, y1: (tc.1 + 1) * TILE }.intersect(area);
        (r.y0..r.y1).any(|y| (r.x0..r.x1).any(|x| tile.0[(((y - tc.1 * TILE) * TILE + (x - tc.0 * TILE)) * 4 + 3) as usize] < ONE as u16))
    })
}

fn decode_jpeg(bytes: &[u8]) -> io::Result<Picture> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let bad = |e: zune_jpeg::errors::DecodeErrors| invalid(trf!("无法解码：{e}", "could not decode: {e}"));
    // 尺寸由下面的 `check_size` 来管，说明是我们自己的话；解码器只拦住 JPEG 本身表示不了的。
    let options = DecoderOptions::default().set_max_width(u16::MAX as usize).set_max_height(u16::MAX as usize).jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(bad)?;
    let info = decoder.info().ok_or_else(|| invalid(tr("无法解码：文件头不完整", "could not decode: incomplete header")))?;
    let (w, h) = (info.width as usize, info.height as usize);
    check_size(w as u32, h as u32)?;
    let exif = decoder.exif().map(|e| read_exif(e)).unwrap_or_default();
    let dpi = jfif_dpi(bytes).or(exif.dpi);
    let rgb = decoder.decode().map_err(bad)?;
    if rgb.len() != w * h * 3 {
        return Err(invalid(tr("图像数据与文件头不符", "image data does not match its header")));
    }
    // 照拍摄方向转正（EXIF 的 1–8）：输出的 (x, y) 取原图的哪一点。5–8 宽高对调。
    let o = exif.orientation;
    let (ow, oh) = if o >= 5 { (h, w) } else { (w, h) };
    let source = |x: usize, y: usize| match o {
        2 => (w - 1 - x, y),
        3 => (w - 1 - x, h - 1 - y),
        4 => (x, h - 1 - y),
        5 => (y, x),
        6 => (y, h - 1 - x),
        7 => (w - 1 - y, h - 1 - x),
        8 => (w - 1 - y, x),
        _ => (x, y),
    };
    let area = Rect { x0: 0, y0: 0, x1: ow as i32, y1: oh as i32 };
    let mut tiles = TileMap::default();
    let mut row: Vec<[u16; 4]> = Vec::with_capacity(ow);
    for y in 0..oh {
        row.clear();
        row.extend((0..ow).map(|x| {
            let (sx, sy) = source(x, y);
            let i = (sy * w + sx) * 3;
            [rgb[i] as u16 * 257, rgb[i + 1] as u16 * 257, rgb[i + 2] as u16 * 257, u16::MAX]
        }));
        store_row(&mut tiles, area, 0, y as i32, &row);
    }
    Ok(Picture { width: ow as i32, height: oh as i32, tiles, dpi, transparent: false })
}

/// EXIF 第 0 个 IFD 里用得上的几项。
#[derive(Clone, Copy)]
struct Exif {
    /// 拍摄方向，1–8，1 是正的
    orientation: u16,
    /// 分辨率（每英寸多少像素）
    dpi: Option<u32>,
}

impl Default for Exif {
    fn default() -> Self {
        Exif { orientation: 1, dpi: None }
    }
}

/// 读 EXIF（从 TIFF 文件头开始；前面带着 `Exif\0\0` 也认）。坏了的地方当作没有。
fn read_exif(data: &[u8]) -> Exif {
    let data = data.strip_prefix(b"Exif\0\0").unwrap_or(data);
    let mut out = Exif::default();
    let little = match data.get(0..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return out,
    };
    let u16_at = |at: usize| data.get(at..at + 2).map(|b| if little { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) });
    let u32_at = |at: usize| {
        data.get(at..at + 4).map(|b| if little { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) })
    };
    if u16_at(2) != Some(42) {
        return out;
    }
    let Some(ifd) = u32_at(4).map(|v| v as usize) else { return out };
    let Some(count) = u16_at(ifd) else { return out };
    // 第 0 个 IFD 里的三项：方向（0x0112，SHORT）、横向分辨率（0x011A，RATIONAL）、分辨率单位（0x0128，SHORT，2 英寸、3 厘米）。
    let (mut resolution, mut unit) = (None, 2);
    for k in 0..count as usize {
        let at = ifd + 2 + k * 12;
        let (Some(tag), Some(kind)) = (u16_at(at), u16_at(at + 2)) else { break };
        match (tag, kind) {
            (0x0112, 3) => out.orientation = u16_at(at + 8).filter(|o| (1..=8).contains(o)).unwrap_or(1),
            (0x0128, 3) => unit = u16_at(at + 8).unwrap_or(2),
            (0x011A, 5) => {
                if let Some(off) = u32_at(at + 8).map(|v| v as usize) {
                    if let (Some(n), Some(d)) = (u32_at(off), u32_at(off + 4)) {
                        resolution = (d != 0).then(|| n as f64 / d as f64);
                    }
                }
            }
            _ => {}
        }
    }
    let dpi = match unit {
        2 => resolution,
        3 => resolution.map(|r| r * 2.54),
        _ => None,
    };
    out.dpi = dpi.map(|d| d.round()).filter(|d| (1.0..=65535.0).contains(d)).map(|d| d as u32);
    out
}

/// JFIF（APP0）里的密度，单位是英寸或厘米时换成每英寸多少像素。
fn jfif_dpi(bytes: &[u8]) -> Option<u32> {
    let mut at = 2;
    // 只看图像数据开始之前的那些段。
    for _ in 0..64 {
        if bytes.get(at) != Some(&0xFF) {
            return None;
        }
        let marker = *bytes.get(at + 1)?;
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let len = u16::from_be_bytes([*bytes.get(at + 2)?, *bytes.get(at + 3)?]) as usize;
        let payload = bytes.get(at + 4..at + 2 + len)?;
        if marker == 0xE0 && payload.starts_with(b"JFIF\0") && payload.len() >= 12 {
            let density = u16::from_be_bytes([payload[8], payload[9]]) as f64;
            let dpi = match payload[7] {
                1 => density,
                2 => density * 2.54,
                _ => return None,
            };
            return Some(dpi.round() as u32).filter(|d| (1..=65535).contains(d));
        }
        at += 2 + len;
    }
    None
}

/// 导出 PNG：整张画布合成后的一张图，每通道 8 位。`transparent` 为 false 时垫上背景色（RGB），
/// 为 true 时不垫（RGBA，没画到的地方透明）。
pub fn export_png(doc: &Document, path: &Path, transparent: bool) -> io::Result<()> {
    let (pixels, color) = if transparent { (doc.flatten_rgba(), png::ColorType::Rgba) } else { (doc.flatten_rgb(), png::ColorType::Rgb) };
    save_atomically(path, |file| {
        let mut out = BufWriter::new(file);
        let mut enc = png::Encoder::new(&mut out, doc.width as u32, doc.height as u32);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        let ppm = png_pixels_per_meter(doc.dpi);
        enc.set_pixel_dims(Some(png::PixelDimensions { xppu: ppm, yppu: ppm, unit: png::Unit::Meter }));
        let mut writer = enc.write_header()?;
        writer.write_image_data(&pixels)?;
        writer.finish()?;
        finish(out)
    })
}

/// 导出 JPEG：整张画布合成后垫上背景色，质量 92，色度不抽样。
pub fn export_jpeg(doc: &Document, path: &Path) -> io::Result<()> {
    let rgb = doc.flatten_rgb();
    save_atomically(path, |file| {
        let mut out = BufWriter::new(file);
        let mut enc = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
        enc.set_density(jpeg_encoder::PixelDensity::dpi(doc.dpi.clamp(1, u16::MAX as u32) as u16));
        // 画布边长不超过 16384，放得进 u16。
        enc.encode(&rgb, doc.width as u16, doc.height as u16, jpeg_encoder::ColorType::Rgb).map_err(io::Error::other)?;
        finish(out)
    })
}

/// 写完：把缓冲里剩下的写出去，落盘。
fn finish(out: BufWriter<File>) -> io::Result<()> {
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::LayerProps;
    use crate::ora::shared::{read_row, to_straight8};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zhibi-image-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn encode_png(w: u32, h: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8], dpi: Option<u32>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        if let Some(d) = dpi {
            let ppm = png_pixels_per_meter(d);
            enc.set_pixel_dims(Some(png::PixelDimensions { xppu: ppm, yppu: ppm, unit: png::Unit::Meter }));
        }
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(data).unwrap();
        wr.finish().unwrap();
        out
    }

    /// 一张 RGBA 8 位的测试图：颜色随位置变，透明度各种值都有（含 0、1、255）。
    fn rgba_pattern(w: usize, h: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let a = [0u8, 1, 2, 128, 254, 255][(x + y) % 6];
                v.extend([(x * 7) as u8, (y * 5) as u8, (x * y) as u8, a]);
            }
        }
        v
    }

    fn straight_at(tiles: &TileMap, x: i32, y: i32) -> [u8; 4] {
        let mut out = [0u8; 4];
        read_row(tiles, y, x, x + 1, &mut out);
        out
    }

    #[test]
    fn png_round_trips_through_open_and_transparent_export() {
        let (w, h) = (130usize, 70usize);
        let src = rgba_pattern(w, h);
        let dir = temp_dir("png-round-trip");
        let path = dir.join("in.png");
        fs::write(&path, encode_png(w as u32, h as u32, png::ColorType::Rgba, png::BitDepth::Eight, &src, Some(144))).unwrap();
        let (doc, transparent) = load(&path).unwrap();
        assert!(transparent);
        assert_eq!((doc.width, doc.height, doc.dpi, doc.layers().len()), (w as i32, h as i32, 144, 1));
        let out = dir.join("out.png");
        export_png(&doc, &out, true).unwrap();
        let back = decode(&fs::read(&out).unwrap()).unwrap();
        assert_eq!(back.dpi, Some(144));
        for y in 0..h {
            for x in 0..w {
                let want = &src[(y * w + x) * 4..][..4];
                let got = straight_at(&back.tiles, x as i32, y as i32);
                match want[3] {
                    // 全透明的像素没有颜色。
                    0 => assert_eq!(got, [0; 4], "({x}, {y})"),
                    // 透明度 1/255 的颜色在预乘格式里只有 130 档，最多差 1（见 `ora::shared` 里的说明）。
                    1 => assert!(got[3] == 1 && (0..3).all(|c| got[c].abs_diff(want[c]) <= 1), "({x}, {y}) {got:?} {want:?}"),
                    _ => assert_eq!(got, [want[0], want[1], want[2], want[3]], "({x}, {y})"),
                }
            }
        }
        // 垫背景色的导出没有透明度：透明的地方是白的。
        export_png(&doc, &out, false).unwrap();
        let flat = decode(&fs::read(&out).unwrap()).unwrap();
        assert!(!flat.transparent);
        assert_eq!(straight_at(&flat.tiles, 0, 0), [255, 255, 255, 255]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sixteen_bit_and_grey_pngs_keep_their_values() {
        // 16 位：0x8000 正好是 15 位里的一半。
        let px16 = [0x80u8, 0x00, 0x40, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];
        let bytes = encode_png(3, 2, png::ColorType::Rgba, png::BitDepth::Sixteen, &px16.repeat(6), None);
        let pic = decode(&bytes).unwrap();
        let t = pic.tiles.get((0, 0)).unwrap();
        assert_eq!(&t.0[0..4], &[16384, 8192, 32768, 32768]);
        assert!(!pic.transparent && pic.dpi.is_none());
        let grey = decode(&encode_png(4, 4, png::ColorType::Grayscale, png::BitDepth::Eight, &[200; 16], None)).unwrap();
        assert_eq!(straight_at(&grey.tiles, 3, 3), [200, 200, 200, 255]);
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        assert!(decode(b"GIF89a....").err().unwrap().to_string().contains("PNG"));
        let mut header = PNG_SIGNATURE.to_vec();
        header.extend([0, 0, 0, 13]);
        header.extend(b"IHDR");
        header.extend((MAX_SIDE as u32 + 1).to_be_bytes());
        header.extend(4u32.to_be_bytes());
        assert!(decode(&header).err().unwrap().to_string().contains("太大"));
        assert!(decode(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 2]).is_err());
    }

    /// 测试用的文档：一层，左半红、右半透明。
    fn half_red(w: i32, h: i32) -> Document {
        let mut tiles = TileMap::default();
        let row: Vec<[u16; 4]> = (0..w).map(|x| if x < w / 2 { [u16::MAX, 0, 0, u16::MAX] } else { [0; 4] }).collect();
        for y in 0..h {
            store_row(&mut tiles, Rect { x0: 0, y0: 0, x1: w, y1: h }, 0, y, &row);
        }
        let layer = Layer::pixels(LayerProps { name: "x".into(), ..Default::default() }, tiles);
        let mut doc = Document::from_layers(w, h, [255; 3], vec![layer]);
        doc.dpi = 300;
        doc
    }

    #[test]
    fn jpeg_export_reads_back_close_to_the_composite() {
        let doc = half_red(200, 120);
        let dir = temp_dir("jpeg");
        let path = dir.join("out.jpg");
        export_jpeg(&doc, &path).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(jfif_dpi(&bytes), Some(300));
        let back = decode(&bytes).unwrap();
        assert_eq!((back.width, back.height, back.dpi, back.transparent), (200, 120, Some(300), false));
        let want = doc.flatten_rgb();
        let mut total = 0u64;
        for y in 0..120 {
            for x in 0..200 {
                let got = straight_at(&back.tiles, x, y);
                let o = (y as usize * 200 + x as usize) * 3;
                total += (0..3).map(|c| (got[c] as i32 - want[o + c] as i32).unsigned_abs() as u64).sum::<u64>();
            }
        }
        let mean = total as f64 / (200.0 * 120.0 * 3.0);
        assert!(mean < 2.0, "平均差 {mean}");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 在一个 JPEG 的 SOI 后面插一段 EXIF（APP1），里面只有方向这一项。
    fn with_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        let mut tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
        tiff.extend(1u16.to_be_bytes());
        tiff.extend(0x0112u16.to_be_bytes());
        tiff.extend(3u16.to_be_bytes());
        tiff.extend(1u32.to_be_bytes());
        tiff.extend(orientation.to_be_bytes());
        tiff.extend([0, 0]);
        tiff.extend(0u32.to_be_bytes());
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend([0xFF, 0xE1]);
        out.extend(((app1.len() + 2) as u16).to_be_bytes());
        out.extend(app1);
        out.extend(&jpeg[2..]);
        out
    }

    #[test]
    fn exif_orientation_turns_the_picture_upright() {
        // 原图 64×32：左上角一块红，别处白。
        let mut rgb = vec![255u8; 64 * 32 * 3];
        for y in 0..8 {
            for x in 0..8 {
                rgb[(y * 64 + x) * 3 + 1] = 0;
                rgb[(y * 64 + x) * 3 + 2] = 0;
            }
        }
        let mut jpeg = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut jpeg, 95);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
        enc.encode(&rgb, 64, 32, jpeg_encoder::ColorType::Rgb).unwrap();
        let red = |p: &Picture, x: i32, y: i32| {
            let c = straight_at(&p.tiles, x, y);
            c[0] > 200 && c[1] < 60 && c[2] < 60
        };
        let plain = decode(&jpeg).unwrap();
        assert_eq!((plain.width, plain.height), (64, 32));
        assert!(red(&plain, 2, 2));
        // 6：要顺时针转 90° 才是正的。转过之后是 32×64，原来的左上角到了右上角。
        let six = decode(&with_orientation(&jpeg, 6)).unwrap();
        assert_eq!((six.width, six.height), (32, 64));
        assert!(red(&six, 29, 2) && !red(&six, 2, 2));
        // 8：逆时针转 90°，原来的左上角到了左下角。
        let eight = decode(&with_orientation(&jpeg, 8)).unwrap();
        assert_eq!((eight.width, eight.height), (32, 64));
        assert!(red(&eight, 2, 61) && !red(&eight, 2, 2));
        // 3：转 180°。
        let three = decode(&with_orientation(&jpeg, 3)).unwrap();
        assert!(red(&three, 61, 29) && !red(&three, 2, 2));
        // 认不出的值当作正的。
        let bad = decode(&with_orientation(&jpeg, 9)).unwrap();
        assert!(red(&bad, 2, 2));
    }

    #[test]
    fn exif_resolution_in_centimetres() {
        let mut tiff = b"II\x2a\0\x08\0\0\0".to_vec();
        tiff.extend(2u16.to_le_bytes());
        // 横向分辨率：118/1 每厘米，数值放在 IFD 后面（8 + 2 + 2·12 + 4 = 38）。
        tiff.extend(0x011Au16.to_le_bytes());
        tiff.extend(5u16.to_le_bytes());
        tiff.extend(1u32.to_le_bytes());
        tiff.extend(38u32.to_le_bytes());
        tiff.extend(0x0128u16.to_le_bytes());
        tiff.extend(3u16.to_le_bytes());
        tiff.extend(1u32.to_le_bytes());
        tiff.extend([3, 0, 0, 0]);
        tiff.extend(0u32.to_le_bytes());
        tiff.extend(118u32.to_le_bytes());
        tiff.extend(1u32.to_le_bytes());
        let exif = read_exif(&tiff);
        assert_eq!((exif.orientation, exif.dpi), (1, Some(300)));
        assert_eq!(read_exif(b"junk").orientation, 1);
    }

    #[test]
    fn transparent_flatten_matches_the_layers() {
        let doc = half_red(100, 10);
        let rgba = doc.flatten_rgba();
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&rgba[99 * 4..100 * 4], &[0, 0, 0, 0]);
        // 第一个分块里每个像素和图层里的一样（只有一层、正常模式、100%）。
        let t = doc.layers()[0].tiles.get((0, 0)).unwrap();
        for x in 0..TILE as usize {
            assert_eq!(&rgba[x * 4..x * 4 + 4], &to_straight8(*t.0[x * 4..].first_chunk().unwrap()));
        }
    }

    /// 窗口测试用的两张图（0.4 的验收），写到 `ZHIBI_INTEROP_DIR`：`photo-rot6.jpg` 是 640×360、左上角一块红、
    /// 带方向 6 的"照片"（打开后应是 360×640 的竖图，红块在右上角）；`cutout.png` 是 300×300 透明底上一个蓝色的圆。
    #[test]
    #[ignore]
    fn write_window_test_pictures() {
        let dir = std::path::PathBuf::from(std::env::var("ZHIBI_INTEROP_DIR").expect("用 ZHIBI_INTEROP_DIR 指定输出目录"));
        fs::create_dir_all(&dir).unwrap();
        let (w, h) = (640usize, 360usize);
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let red = x < 120 && y < 120;
                rgb.extend(if red { [230, 30, 30] } else { [(x * 255 / w) as u8, 200, (y * 255 / h) as u8] });
            }
        }
        let mut jpeg = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut jpeg, 90);
        enc.set_density(jpeg_encoder::PixelDensity::dpi(150));
        enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).unwrap();
        fs::write(dir.join("photo-rot6.jpg"), with_orientation(&jpeg, 6)).unwrap();
        let n = 300usize;
        let mut rgba = Vec::with_capacity(n * n * 4);
        for y in 0..n {
            for x in 0..n {
                let d = ((x as f64 - 150.0).powi(2) + (y as f64 - 150.0).powi(2)).sqrt();
                let a = ((120.0 - d).clamp(0.0, 1.0) * 255.0) as u8;
                rgba.extend([30, 80, 220, a]);
            }
        }
        fs::write(dir.join("cutout.png"), encode_png(n as u32, n as u32, png::ColorType::Rgba, png::BitDepth::Eight, &rgba, Some(72))).unwrap();
    }

    /// 分析参照软件文件格式用的两个样本（F-146，功能列表 §18），写到 `ZHIBI_INTEROP_DIR`：
    /// `blocks.png` 是 256×96、8×3 个 32×32 的块，每块一种已知的图案；`layers.psd` 是各种图层属性都有的分层文件。
    /// 用户在参照软件里打开它们、另存为它的格式，拿回来对着已知的内容推出格式。
    #[test]
    #[ignore]
    fn write_format_samples() {
        use crate::document::{BlendMode, Mask};
        let dir = std::path::PathBuf::from(std::env::var("ZHIBI_INTEROP_DIR").expect("用 ZHIBI_INTEROP_DIR 指定输出目录"));
        fs::create_dir_all(&dir).unwrap();
        let mut seed = 12345u32;
        let mut rand = move || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        let (w, h) = (256usize, 96usize);
        let mut rgba = vec![0u8; w * h * 4];
        for b in 0..24usize {
            let (bx, by) = ((b % 8) * 32, (b / 8) * 32);
            for y in 0..32usize {
                for x in 0..32usize {
                    let v = |n: usize| (n.min(255)) as u8;
                    let px: [u8; 4] = match b {
                        0 => if (x, y) == (0, 0) { [255, 0, 0, 255] } else { [0; 4] },
                        1 => if (x, y) == (5, 7) { [0, 255, 0, 255] } else { [0; 4] },
                        2 => if (x, y) == (31, 31) { [0, 0, 255, 128] } else { [0; 4] },
                        3 => [v(x * 8), 0, 0, 255],
                        4 => [0, v(y * 8), 0, 255],
                        5 => if (x + y) % 2 == 0 { [0, 0, 0, 255] } else { [255, 255, 255, 255] },
                        6 => if (x, y) == (10, 10) { [101, 150, 200, 255] } else { [100, 150, 200, 255] },
                        7 => [255, 255, 255, v(x * 8)],
                        8 => [v((x + y) * 4), ((x * y) & 255) as u8, v(255 - x * 8), 255],
                        9 | 16..=20 => [rand(), rand(), rand(), 255],
                        10 | 21..=23 => [rand(), rand(), rand(), rand()],
                        11 => [rand() & 3, rand() & 3, rand() & 3, 255],
                        12 => if y == 16 { [0, 0, 0, 255] } else { [255; 4] },
                        13 => if x == 16 { [0, 0, 0, 255] } else { [255; 4] },
                        14 => if y == 0 { [255; 4] } else { [0; 4] },
                        _ => if x == 0 { [255; 4] } else { [0; 4] },
                    };
                    let o = ((by + y) * w + bx + x) * 4;
                    rgba[o..o + 4].copy_from_slice(&px);
                }
            }
        }
        fs::write(dir.join("blocks.png"), encode_png(w as u32, h as u32, png::ColorType::Rgba, png::BitDepth::Eight, &rgba, Some(72))).unwrap();

        // 分层的样本：自下而上；深度 1 的在紧跟着的文件夹里。
        let canvas = Rect { x0: 0, y0: 0, x1: 256, y1: 128 };
        let rect = |x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4]| {
            let mut tiles = TileMap::default();
            let row: Vec<[u16; 4]> = (x0..x1).map(|_| c.map(|v| v as u16 * 257)).collect();
            for y in y0..y1 {
                store_row(&mut tiles, canvas, x0 as i64, y, &row);
            }
            tiles
        };
        let layer = |name: &str, depth: u16, props: LayerProps, tiles: TileMap| {
            let mut l = Layer::pixels(LayerProps { name: name.into(), ..props }, tiles);
            l.depth = depth;
            l
        };
        let d = LayerProps::default;
        let mut masked = layer("中文名 mask", 0, d(), rect(150, 70, 250, 120, [40, 160, 60, 255]));
        let mut mask = Mask::white();
        for y in 70..120 {
            for x in 150..200 {
                let t = mask.tiles.get_mut((x / TILE, y / TILE), ONE as u16);
                t[((y % TILE) * TILE + x % TILE) as usize] = 0;
            }
        }
        masked.mask = Some(mask);
        let mut folder_a = Layer::folder("组 A");
        folder_a.props.pass_through = false;
        folder_a.props.opacity = 0.8;
        let mut folder_b = Layer::folder("Group B");
        folder_b.props.pass_through = true;
        let layers = vec![
            layer("bottom", 0, d(), rect(0, 0, 256, 128, [200, 190, 170, 255])),
            layer("multiply 37", 0, LayerProps { blend: BlendMode::Multiply, opacity: 0.37, ..d() }, rect(10, 10, 120, 60, [220, 30, 30, 255])),
            layer("screen", 0, LayerProps { blend: BlendMode::Screen, ..d() }, rect(60, 30, 180, 90, [30, 30, 220, 255])),
            layer("hidden", 0, LayerProps { visible: false, ..d() }, rect(0, 100, 50, 128, [0, 0, 0, 255])),
            layer("clipped", 0, LayerProps { clip: true, ..d() }, rect(0, 0, 256, 20, [250, 250, 0, 255])),
            layer("lock alpha", 0, LayerProps { lock_alpha: true, ..d() }, rect(200, 5, 250, 40, [0, 200, 200, 128])),
            layer("inside A overlay", 1, LayerProps { blend: BlendMode::Overlay, ..d() }, rect(20, 70, 100, 120, [120, 60, 200, 255])),
            folder_a,
            layer("inside B add", 1, LayerProps { blend: BlendMode::Add, ..d() }, rect(100, 70, 140, 120, [60, 60, 60, 255])),
            folder_b,
            masked,
        ];
        let mut doc = Document::from_layers(256, 128, [255; 3], layers);
        doc.dpi = 300;
        crate::psd::save(&doc, &dir.join("layers.psd")).unwrap();

        // 第三个样本 `modes.psd`：每种混合模式一层（名字就是模式的编号），外加套了两层的组、带蒙版的组、
        // 停用的蒙版、伸出画布左边的内容。
        // 这个画布是 256×256，伸出画布的也要留着，所以换一个范围大的 `rect`。
        let rect = |x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4]| {
            let area = Rect { x0: -256, y0: -256, x1: 512, y1: 512 };
            let mut tiles = TileMap::default();
            let row: Vec<[u16; 4]> = (x0..x1).map(|_| c.map(|v| v as u16 * 257)).collect();
            for y in y0..y1 {
                store_row(&mut tiles, area, x0 as i64, y, &row);
            }
            tiles
        };
        let mut layers = vec![layer("bottom", 0, d(), rect(0, 0, 256, 256, [90, 140, 200, 255]))];
        for (k, &blend) in BlendMode::ALL.iter().enumerate() {
            let (x, y) = ((k % 8) as i32 * 32, (k / 8) as i32 * 32);
            layers.push(layer(&format!("m{k:02} {}", blend.id()), 0, LayerProps { blend, ..d() }, rect(x, y, x + 30, y + 30, [200, 100, 50, 255])));
        }
        let mut off_mask = Mask::white();
        off_mask.tiles.get_mut((0, 2), ONE as u16)[0] = 0;
        off_mask.disabled = true;
        let mut disabled = layer("mask disabled", 0, d(), rect(0, 160, 64, 200, [10, 200, 10, 255]));
        disabled.mask = Some(off_mask);
        layers.push(disabled);
        layers.push(layer("off canvas", 0, d(), rect(-100, 200, 40, 250, [250, 0, 250, 255])));
        layers.push(layer("deep", 2, d(), rect(100, 200, 140, 240, [0, 0, 0, 255])));
        let mut inner = Layer::folder("inner");
        inner.depth = 1;
        layers.push(inner);
        let mut outer = Layer::folder("outer");
        let mut fmask = Mask::white();
        for y in 200..256 {
            for x in 120..256 {
                fmask.tiles.get_mut((x / TILE, y / TILE), ONE as u16)[((y % TILE) * TILE + x % TILE) as usize] = 0;
            }
        }
        outer.mask = Some(fmask);
        layers.push(outer);
        let mut doc = Document::from_layers(256, 256, [255; 3], layers);
        doc.dpi = 300;
        crate::psd::save(&doc, &dir.join("modes.psd")).unwrap();
    }
}
