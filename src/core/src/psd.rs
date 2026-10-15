// SPDX-License-Identifier: GPL-3.0-or-later
//! Photoshop（.psd）的读写，只覆盖 8 位 RGB 的普通像素图层。
//!
//! 按 Adobe 公开的《Adobe Photoshop File Formats Specification》实现。文件里所有的数都是大端的，依次是五段：
//!
//! 1. 文件头：签名 `8BPS`、版本、通道数、高、宽、位深、颜色模式；
//! 2. 颜色模式数据（RGB 模式没有）；
//! 3. 图像资源：这里只用到分辨率（1005 号）；
//! 4. 图层与蒙版信息：每个图层一条记录（自下而上），后面跟着各图层各通道的像素，一个通道一个平面；
//! 5. 合并图像：整张画布合成之后的像素，不认识图层的程序看的就是它。
//!
//! 写出去的文件是版本 1、每通道 8 位、RGB 模式，像素用 PackBits（RLE）压缩。和 OpenRaster 一样，这是
//! 交换用的格式，存一趟会丢掉本程序 15 位精度里多出来的那部分；背景色也是写成最底下一个名叫"背景"
//! （英文界面下叫"Background"）的不透明图层，读的时候再还原（见 `ora` 模块的说明）。
//!
//! 图层组读写成文件夹（规格 F-055），图层蒙版读写成蒙版（规格 F-058），填充图层读写成填充图层（规格 F-061）。
//! 读的时候，PSD 里本程序没有的东西（矢量蒙版、调整图层、图层样式……）要么跳过、要么忽略，
//! 并且在返回的说明里一一交代，见 [`load`]。

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::document::{BlendMode, Document, Fill, Kind, Layer, LayerProps, Mask, Paint, MAX_SIDE};
use crate::style::{Effect, EffectKind, Style};
use crate::lang::tr;
use crate::ora::shared::{
    self, background_name, check_document, content_bounds, invalid, read_row, save_atomically, store_row, take_background,
    Notes, MAX_LAYERS,
};
use crate::tile::{GrayMap, TileMap, TILE};
use crate::trf;
use crate::{Rect, ONE};

/// 文件扩展名（不含点）
pub const EXTENSION: &str = "psd";

const SIGNATURE: &[u8; 4] = b"8BPS";
/// 图像资源块、混合模式、附加图层信息块共用的签名。
const BLOCK_SIGNATURE: &[u8; 4] = b"8BIM";
/// 文件头里"颜色模式"一项的值：RGB。
const MODE_RGB: u16 = 3;
/// 图像资源的编号：分辨率（ResolutionInfo）。
const RESOURCE_RESOLUTION: u16 = 1005;
/// 图像资源：全局光的角度、高度（各 4 个字节的整数，度）。图层样式里"使用全局光"的效果用它们。
const RESOURCE_GLOBAL_ANGLE: u16 = 1037;
const RESOURCE_GLOBAL_ALTITUDE: u16 = 1049;
/// 文件里没写分辨率时的值，和 Photoshop 新建文档时的默认值一样。
const DEFAULT_DPI: u32 = 72;
/// 图层记录里"标志"字节的第 1 位。规范把它叫作"可见"，实际的文件里却是图层**隐藏**时这一位才是 1
/// （规范这一处写反了）。用 GIMP 核对过：它导出的文件里隐藏的图层这一位是 1，我们这样写的文件它也读成隐藏。
/// 保护不透明度（规范里叫 transparency protected）
const FLAG_KEEP_ALPHA: u8 = 1;
const FLAG_HIDDEN: u8 = 1 << 1;
/// "标志"字节的第 3 位：Photoshop 5.0 以后的文件都置 1，表示第 4 位有效。
const FLAG_BIT4_VALID: u8 = 1 << 3;
/// "标志"字节的第 4 位：这个图层的像素与文档的外观无关。图层组的记录、调整图层、填充图层都是这样。
const FLAG_IRRELEVANT: u8 = 1 << 4;
/// 写出去的图层的四个通道：编号（-1 是透明度，0、1、2 是红、绿、蓝）和它在 RGBA 像素里的位置。
const CHANNELS: [(i16, usize); 4] = [(-1, 3), (0, 0), (1, 1), (2, 2)];
/// 一条图层记录里通道个数的上限。规范说整个文档最多 56 个通道，再加上蒙版也多不到哪里去。
const MAX_CHANNELS: usize = 64;
/// 图层矩形边长的上限。图层可以比画布大（多出来的部分读入时裁掉），但规范里最大的文档（PSB）
/// 也只有 300000 像素见方，超过它的只可能是坏文件。
const MAX_LAYER_SIDE: i64 = 300_000;
/// 图层组的分隔记录（`lsct` 块）的类型：1、2 是组本身（展开的、折叠的），3 是标出组从哪里开始的隐藏记录。
const SECTION_OPEN: u32 = 1;
const SECTION_CLOSED: u32 = 2;
const SECTION_END: u32 = 3;
/// 标出组从哪里开始的那条隐藏记录的名字（Photoshop 自己就是这么写的）。
const SECTION_END_NAME: &str = "</Layer group>";
/// 组的混合键"穿透"。
const PASS_THROUGH: &[u8; 4] = b"pass";
/// 图层蒙版的通道编号；同时有矢量蒙版时，用户画的那个蒙版在 −3 号通道。
const MASK_CHANNEL: i16 = -2;
const REAL_MASK_CHANNEL: i16 = -3;
/// 蒙版数据里"标志"字节的第 1 位：蒙版停用。
const MASK_DISABLED: u8 = 1 << 1;
/// 蒙版数据里"标志"字节的第 4 位：后面跟着浓度、羽化这些参数。
const MASK_HAS_PARAMS: u8 = 1 << 4;

/// 混合模式在文件里的四字符代号。
const BLEND_KEYS: [(BlendMode, &[u8; 4]); 8] = [
    (BlendMode::Normal, b"norm"),
    (BlendMode::Multiply, b"mul "),
    (BlendMode::Screen, b"scrn"),
    (BlendMode::Overlay, b"over"),
    (BlendMode::Darken, b"dark"),
    (BlendMode::Lighten, b"lite"),
    (BlendMode::Add, b"lddg"),
    (BlendMode::Difference, b"diff"),
];

fn blend_key(mode: BlendMode) -> &'static [u8; 4] {
    BLEND_KEYS.iter().find(|(m, _)| *m == mode).map_or(b"norm", |(_, key)| key)
}

fn blend_of(key: &[u8; 4]) -> Option<BlendMode> {
    BLEND_KEYS.iter().find(|(_, k)| *k == key).map(|(m, _)| *m)
}

// ---- PackBits ----
//
// PSD 里的"RLE"就是 Apple 的 PackBits，每一行单独压缩。数据是一段一段的，每段开头一个有符号的字节 n：
// - 0 ≤ n ≤ 127：后面 n+1 个字节原样照抄；
// - −127 ≤ n ≤ −1：后面一个字节重复 1−n 次；
// - n = −128：什么也不做。

/// 用 PackBits 压缩一行，结果接在 `out` 后面。最坏的情况下比原来多出 ⌈长度/128⌉ 个字节。
fn pack_bits(row: &[u8], out: &mut Vec<u8>) {
    // 原样照抄的一段最多 128 个字节。
    let literal = |bytes: &[u8], out: &mut Vec<u8>| {
        for chunk in bytes.chunks(128) {
            out.push((chunk.len() - 1) as u8);
            out.extend_from_slice(chunk);
        }
    };
    // `pending..i` 是攒着还没写出去的、要原样照抄的字节。
    let (mut pending, mut i) = (0, 0);
    while i < row.len() {
        let run = row[i..].iter().take(128).take_while(|&&b| b == row[i]).count();
        // 连续三个以上相同才值得写成"重复"；两个相同的写成"重复"和照抄一样长，只有前面没攒着东西时
        // 才这么写，免得把一段照抄拆成两段、反而多出一个字节。
        if run >= 3 || (run == 2 && pending == i) {
            literal(&row[pending..i], out);
            out.push((1 - run as i32) as u8);
            out.push(row[i]);
            i += run;
            pending = i;
        } else {
            i += run;
        }
    }
    literal(&row[pending..], out);
}

/// 解开一行 PackBits 数据，要正好填满 `out`。填满之后多出来的输入不管它。
fn unpack_bits(mut src: &[u8], out: &mut [u8]) -> Result<(), &'static str> {
    let short = rle_incomplete;
    let long = || tr("RLE 数据解出来比一行还长", "RLE data unpacks to more than a row");
    let mut at = 0;
    while at < out.len() {
        let (&head, rest) = src.split_first().ok_or_else(short)?;
        src = rest;
        let space = out.len() - at;
        match head as i8 {
            -128 => {}
            n if n >= 0 => {
                let len = n as usize + 1;
                let (bytes, rest) = src.split_at_checked(len).ok_or_else(short)?;
                if len > space {
                    return Err(long());
                }
                out[at..at + len].copy_from_slice(bytes);
                src = rest;
                at += len;
            }
            n => {
                let len = (1 - n as i32) as usize;
                let (&value, rest) = src.split_first().ok_or_else(short)?;
                if len > space {
                    return Err(long());
                }
                out[at..at + len].fill(value);
                src = rest;
                at += len;
            }
        }
    }
    Ok(())
}

/// RLE 数据不够解出一整行时的说明。
fn rle_incomplete() -> &'static str {
    tr("RLE 数据不完整", "RLE data is incomplete")
}

// ---- 保存 ----

/// 把文档存成 PSD 文件。
///
/// 和 `format::save` 一样，先写进同一目录下的临时文件，完整落盘之后再改名顶替 `path`。
/// 不会把文档标成"已保存"：这种格式存不下文档的全部内容（见模块说明），算不算保存过由调用方决定。
pub fn save(doc: &Document, path: &Path) -> io::Result<()> {
    check_document(doc)?;
    save_atomically(path, |file| write_file(doc, file))
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend(v.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend(v.to_be_bytes());
}

/// 版本 1 的 PSD 里长度都是 32 位的，装不下就存不成。
fn length(n: u64) -> io::Result<u32> {
    u32::try_from(n).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            tr("文档太大，超出了 PSD 格式 4 GB 的上限", "The document is too large for the 4 GB limit of the PSD format"),
        )
    })
}

fn write_file(doc: &Document, file: File) -> io::Result<()> {
    let canvas = doc.bounds();
    let (w, h) = (doc.width as usize, doc.height as usize);

    // `front` 是文件开头直到最后一条图层记录的部分。里面有几个长度要等像素压缩完才知道，
    // 先空着，像素写完之后填上，再回到文件开头把这一整段重写一遍。
    let mut front = Vec::new();
    front.extend(SIGNATURE);
    put_u16(&mut front, 1); // 版本
    front.extend([0; 6]); // 保留
    put_u16(&mut front, 3); // 合并图像的通道数：红、绿、蓝
    put_u32(&mut front, h as u32);
    put_u32(&mut front, w as u32);
    put_u16(&mut front, 8); // 每通道的位数
    put_u16(&mut front, MODE_RGB);
    put_u32(&mut front, 0); // 颜色模式数据：RGB 模式没有

    // 图像资源：只有分辨率一项。结构是 Photoshop API 文档里的 ResolutionInfo：
    // 横向分辨率（16.16 定点，像素/英寸）、它的显示单位（1 = 像素/英寸）、宽度的显示单位（1 = 英寸），纵向同样三项。
    let dpi = doc.dpi.clamp(1, i16::MAX as u32);
    let mut resource = Vec::new();
    for _ in 0..2 {
        put_u32(&mut resource, dpi << 16);
        put_u16(&mut resource, 1);
        put_u16(&mut resource, 1);
    }
    // 图层样式从 PSD 读进来时带着文件的全局光，原样写回去，"使用全局光"的效果在 PS 里方向才不变。
    let light = doc.layers().iter().find_map(|l| l.style.as_ref().and_then(|s| s.light));
    let mut resources = vec![(RESOURCE_RESOLUTION, resource)];
    if let Some((angle, altitude)) = light {
        resources.push((RESOURCE_GLOBAL_ANGLE, angle.to_be_bytes().to_vec()));
        resources.push((RESOURCE_GLOBAL_ALTITUDE, altitude.to_be_bytes().to_vec()));
    }
    put_u32(&mut front, resources.iter().map(|(_, d)| 12 + d.len() as u32).sum());
    for (id, data) in resources {
        front.extend(BLOCK_SIGNATURE);
        put_u16(&mut front, id);
        put_u16(&mut front, 0); // 资源的名字：空的 Pascal 字符串，补成偶数长度
        put_u32(&mut front, data.len() as u32);
        front.extend(data);
    }

    // 图层与蒙版信息。最底下多写一个图层，代表画布的背景色；文件夹里的东西前面先放一条"组从这里开始"的记录。
    let records = plan_records(doc, canvas);
    if records.len() > MAX_LAYERS {
        let n = records.len();
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            trf!("图层和组太多（连同分隔记录 {n} 条），PSD 最多存 {MAX_LAYERS} 条", "Too many layers and groups ({n} records including group markers); PSD holds at most {MAX_LAYERS}"),
        ));
    }
    let section_at = front.len();
    put_u32(&mut front, 0); // 这一整段的长度，后填
    put_u32(&mut front, 0); // 其中"图层信息"的长度，后填
    let info_start = front.len();
    put_u16(&mut front, records.len() as u16);
    let lengths_at: Vec<usize> = records.iter().map(|r| push_record(&mut front, r)).collect();

    let mut out = BufWriter::new(file);
    out.write_all(&front)?;
    let mut info_len = (front.len() - info_start) as u64;
    for (record, lengths_at) in records.iter().zip(lengths_at) {
        let mut channels: Vec<Vec<u8>> = match record.pixels {
            Pixels::Color([r, g, b]) => encode_channels(record.rect, |_, row| row.as_chunks_mut::<4>().0.fill([r, g, b, 255]))?,
            Pixels::Tiles(tiles) => encode_channels(record.rect, |y, row| read_row(tiles, y, record.rect.x0, record.rect.x1, row))?,
            Pixels::Nothing => encode_channels(Rect::EMPTY, |_, _| ())?,
        }
        .into();
        if let Some((mask, r)) = record.mask {
            channels.push(encode_mask(mask, r)?);
        }
        for (k, data) in channels.iter().enumerate() {
            let at = lengths_at + k * 6;
            front[at..at + 4].copy_from_slice(&length(data.len() as u64)?.to_be_bytes());
            out.write_all(data)?;
            info_len += data.len() as u64;
        }
    }
    // "图层信息"的长度要是偶数。
    if info_len % 2 == 1 {
        out.write_all(&[0])?;
        info_len += 1;
    }
    out.write_all(&[0; 4])?; // 全局图层蒙版信息：没有
    front[section_at + 4..section_at + 8].copy_from_slice(&length(info_len)?.to_be_bytes());
    front[section_at..section_at + 4].copy_from_slice(&length(info_len + 8)?.to_be_bytes());

    // 合并图像。
    let rgb = doc.flatten_rgb();
    out.write_all(&encode_merged(&rgb, w, h)?)?;
    drop(rgb);

    out.seek(SeekFrom::Start(0))?;
    out.write_all(&front)?;
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

/// 一条图层记录的像素从哪里来。
enum Pixels<'a> {
    /// 整块一个颜色（代表背景的那个图层）
    Color([u8; 3]),
    Tiles(&'a TileMap),
    /// 没有像素（文件夹、组的分隔记录）
    Nothing,
}

/// 要写进文件的一条图层记录。
struct RecordOut<'a> {
    name: String,
    rect: Rect,
    blend: [u8; 4],
    opacity: f32,
    clip: bool,
    flags: u8,
    pixels: Pixels<'a>,
    /// 蒙版和它写进文件的矩形
    mask: Option<(&'a Mask, Rect)>,
    /// 附加信息块（`luni` 之外的）
    blocks: Vec<([u8; 4], Vec<u8>)>,
}

/// 文件里自下而上的全部图层记录：背景，然后各图层；每个文件夹里的东西前面先放一条"组从这里开始"的记录，
/// 嵌套的组外层的在前；文件夹自己的记录跟在它里面的东西后面（实现方案 §3.11 的存法和 PSD 是一样的）。
fn plan_records(doc: &Document, canvas: Rect) -> Vec<RecordOut<'_>> {
    let layers = doc.layers();
    // 伸出画布的内容照写（实现方案 §3.15）。
    let keep = doc.keep_bounds();
    let mut starts: Vec<Vec<usize>> = vec![Vec::new(); layers.len()];
    for g in (0..layers.len()).filter(|&g| layers[g].is_folder()) {
        starts[doc.subtree(g).start].push(g);
    }
    let background = RecordOut {
        name: background_name().into(),
        rect: canvas,
        blend: *blend_key(BlendMode::Normal),
        opacity: 1.0,
        clip: false,
        flags: FLAG_BIT4_VALID,
        pixels: Pixels::Color(doc.background),
        mask: None,
        blocks: Vec::new(),
    };
    let mut out = vec![background];
    for (i, layer) in layers.iter().enumerate() {
        starts[i].sort_by_key(|&g| layers[g].depth);
        for _ in &starts[i] {
            out.push(RecordOut {
                name: SECTION_END_NAME.into(),
                rect: Rect::EMPTY,
                blend: *blend_key(BlendMode::Normal),
                opacity: 1.0,
                clip: false,
                flags: FLAG_BIT4_VALID | FLAG_IRRELEVANT,
                pixels: Pixels::Nothing,
                mask: None,
                blocks: vec![(*b"lsct", section(SECTION_END, None))],
            });
        }
        let p = &layer.props;
        let mut flags = FLAG_BIT4_VALID | if p.visible { 0 } else { FLAG_HIDDEN } | if p.lock_alpha { FLAG_KEEP_ALPHA } else { 0 };
        let mut record = RecordOut {
            name: p.name.clone(),
            rect: Rect::EMPTY,
            blend: *blend_key(p.blend),
            opacity: p.opacity,
            clip: p.clip,
            flags,
            pixels: Pixels::Nothing,
            mask: layer.mask.as_ref().map(|m| (m, mask_rect(m, keep, canvas))),
            blocks: Vec::new(),
        };
        match &layer.kind {
            Kind::Pixels => {
                record.rect = content_bounds(&layer.tiles, keep);
                record.pixels = Pixels::Tiles(&layer.tiles);
            }
            Kind::Folder { open } => {
                flags |= FLAG_IRRELEVANT;
                if p.pass_through {
                    record.blend = *PASS_THROUGH;
                }
                let kind = if *open { SECTION_OPEN } else { SECTION_CLOSED };
                record.blocks.push((*b"lsct", section(kind, Some(&record.blend))));
            }
            Kind::Fill(fill) => {
                // 填充图层也写上画好的像素：PS 按描述符重画，不看它；不认识填充图层的程序（例如 GIMP）就显示这些像素。
                // 纯色铺满画布；还不会画的那些，文件里存着的像素原样写回去。
                match fill.paint {
                    Paint::Solid(c) => {
                        record.rect = canvas;
                        record.pixels = Pixels::Color(c);
                    }
                    Paint::Other => {
                        record.rect = content_bounds(&layer.tiles, keep);
                        record.pixels = Pixels::Tiles(&layer.tiles);
                    }
                }
                match (&fill.psd, &fill.paint) {
                    (Some((key, data)), _) => record.blocks.push((*key, data.clone())),
                    (None, Paint::Solid(c)) => record.blocks.push((*b"SoCo", solid_color_descriptor(*c))),
                    (None, Paint::Other) => {}
                }
            }
        }
        // 图层样式：PS 原来的块原样写回去（总开关改过的已经改在里面了）。
        if let Some(style) = &layer.style {
            record.blocks.extend(style.psd.iter().cloned());
        }
        record.flags = flags;
        out.push(record);
    }
    out
}

/// `lsct` 块的数据：分隔记录的类型，组本身后面再跟签名和混合键。
fn section(kind: u32, blend: Option<&[u8; 4]>) -> Vec<u8> {
    let mut data = kind.to_be_bytes().to_vec();
    if let Some(key) = blend {
        data.extend(BLOCK_SIGNATURE);
        data.extend(key);
    }
    data
}

/// 蒙版写进文件的矩形：蒙版存着的分块的范围，裁到画布里。整个是默认值的蒙版写成整张画布大小
/// （有的程序读到空矩形的蒙版就不认），RLE 压缩之后也没多大。
fn mask_rect(mask: &Mask, keep: Rect, canvas: Rect) -> Rect {
    // 和默认值不一样的那些点的范围：分块里等于默认值的地方不必写，读回来再存一次也还是同一个矩形。
    let mut r = Rect::EMPTY;
    for (tx, ty) in mask.tiles.coords() {
        let whole = Rect { x0: tx * TILE, y0: ty * TILE, x1: (tx + 1) * TILE, y1: (ty + 1) * TILE };
        let area = whole.intersect(keep);
        if area.is_empty() || r.union(area) == r {
            continue;
        }
        let t = mask.tiles.get((tx, ty)).unwrap();
        for y in area.y0..area.y1 {
            let differs = |x: i32| t[((y - whole.y0) * TILE + (x - whole.x0)) as usize] != mask.default;
            let Some(first) = (area.x0..area.x1).find(|&x| differs(x)) else { continue };
            let last = (area.x0..area.x1).rev().find(|&x| differs(x)).unwrap_or(first);
            r = r.union(Rect { x0: first, y0: y, x1: last + 1, y1: y + 1 });
        }
    }
    if r.is_empty() {
        canvas
    } else {
        r
    }
}

/// 一个纯色填充图层的描述符（`SoCo` 块的数据）：版本 16，然后是一个类为 `null` 的描述符，
/// 里面一项 `Clr `，是一个 `RGBC` 类的对象，三项 `Rd  `、`Grn `、`Bl  `（0 到 255 的双精度数）。
fn solid_color_descriptor(c: [u8; 3]) -> Vec<u8> {
    let mut d = Vec::new();
    let id = |d: &mut Vec<u8>, key: &[u8; 4]| {
        put_u32(d, 0);
        d.extend(key);
    };
    put_u32(&mut d, 16);
    // 描述符的名字：Unicode 字符串，Photoshop 写的是只有一个结尾 0 的空串。
    put_u32(&mut d, 1);
    put_u16(&mut d, 0);
    id(&mut d, b"null");
    put_u32(&mut d, 1);
    id(&mut d, b"Clr ");
    d.extend(b"Objc");
    put_u32(&mut d, 1);
    put_u16(&mut d, 0);
    id(&mut d, b"RGBC");
    put_u32(&mut d, 3);
    for (key, v) in [(b"Rd  ", c[0]), (b"Grn ", c[1]), (b"Bl  ", c[2])] {
        id(&mut d, key);
        d.extend(b"doub");
        d.extend((v as f64).to_bits().to_be_bytes());
    }
    d
}

/// 往 `out` 里加一条图层记录。各通道的"数据长度"先写 0，返回第一个通道的这个字段的位置
/// （后面每个通道的依次往后 6 个字节）。通道依次是 `CHANNELS` 里的四个，有蒙版的再加一个 −2。
fn push_record(out: &mut Vec<u8>, record: &RecordOut) -> usize {
    // 矩形：上、左、下、右。
    let rect = record.rect;
    for v in [rect.y0, rect.x0, rect.y1, rect.x1] {
        out.extend(v.to_be_bytes());
    }
    let ids: Vec<i16> = CHANNELS.iter().map(|c| c.0).chain(record.mask.map(|_| MASK_CHANNEL)).collect();
    put_u16(out, ids.len() as u16);
    let lengths_at = out.len() + 2;
    for id in ids {
        out.extend(id.to_be_bytes());
        put_u32(out, 0);
    }
    out.extend(BLOCK_SIGNATURE);
    out.extend(record.blend);
    // 合成时非数当 0 用，这里保持一致。
    let opacity = if record.opacity.is_nan() { 0.0 } else { record.opacity.clamp(0.0, 1.0) };
    out.push((opacity * 255.0).round() as u8);
    out.push(record.clip as u8); // 剪贴：0 是普通图层，1 是剪贴在下面那一层上
    out.push(record.flags);
    out.push(0); // 填充字节

    let mut extra = Vec::new();
    // 图层蒙版数据：矩形（上、左、下、右）、矩形以外的颜色（0 或 255）、标志，再补两个字节凑成 20。
    match record.mask {
        Some((mask, r)) => {
            put_u32(&mut extra, 20);
            for v in [r.y0, r.x0, r.y1, r.x1] {
                extra.extend(v.to_be_bytes());
            }
            extra.push(if mask.default as u32 >= ONE / 2 { 255 } else { 0 });
            extra.push(if mask.disabled { MASK_DISABLED } else { 0 });
            extra.extend([0, 0]);
        }
        None => put_u32(&mut extra, 0),
    }
    // 混合范围："合成灰度"和四个通道各一组，每组是源和目标的范围，都取全范围（黑 0、0，白 255、255）。
    put_u32(&mut extra, 5 * 8);
    for _ in 0..5 * 2 {
        extra.extend([0, 0, 255, 255]);
    }
    // 图层名，Pascal 字符串（一个字节的长度加内容），连长度字节一起补到 4 的倍数。它最多 255 个字节，
    // 编码也没有定准，现在的程序都以后面的 Unicode 名字为准；这里放的是 UTF-8，太长就在字符的边界上截断。
    let name = &record.name;
    let mut end = name.len().min(255);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    extra.push(end as u8);
    extra.extend(&name.as_bytes()[..end]);
    extra.resize(extra.len().next_multiple_of(4), 0);
    // 附加信息块 `luni`：完整的图层名。四个字节的字符数（UTF-16 的码元数），然后是 UTF-16 的字符；
    // 整块补到 4 的倍数。
    let units: Vec<u16> = name.encode_utf16().collect();
    let len = (4 + units.len() * 2).next_multiple_of(4);
    extra.extend(BLOCK_SIGNATURE);
    extra.extend(b"luni");
    put_u32(&mut extra, len as u32);
    put_u32(&mut extra, units.len() as u32);
    extra.extend(units.iter().flat_map(|u| u.to_be_bytes()));
    extra.resize(extra.len().next_multiple_of(4), 0);
    // 其余的附加信息块（组的分隔、填充图层的描述符），数据补到 4 的倍数，长度连补的一起算。
    for (key, data) in &record.blocks {
        extra.extend(BLOCK_SIGNATURE);
        extra.extend(key);
        put_u32(&mut extra, data.len().next_multiple_of(4) as u32);
        extra.extend(data);
        extra.resize(extra.len().next_multiple_of(4), 0);
    }

    put_u32(out, extra.len() as u32);
    out.extend(extra);
    lengths_at
}

/// 蒙版在矩形 `r` 里的值压缩成一个通道的数据：8 位（四舍五入），RLE。
fn encode_mask(mask: &Mask, r: Rect) -> io::Result<Vec<u8>> {
    let (w, h) = ((r.x1 - r.x0) as usize, (r.y1 - r.y0) as usize);
    let mut data = vec![0u8; 2 + h * 2];
    data[1] = 1;
    let mut plane = vec![0u8; w];
    for (i, y) in (r.y0..r.y1).enumerate() {
        for (k, p) in plane.iter_mut().enumerate() {
            let x = r.x0 + k as i32;
            let (tx, ty) = (x.div_euclid(TILE), y.div_euclid(TILE));
            let v = mask.tiles.get((tx, ty)).map_or(mask.default, |t| t[((y - ty * TILE) * TILE + (x - tx * TILE)) as usize]);
            *p = (((v as u32).min(ONE) * 255 + ONE / 2) >> 15) as u8;
        }
        pack_row(&plane, &mut data, 2 + i * 2)?;
    }
    Ok(data)
}

/// 把一个图层在矩形 `rect` 里的像素压缩成四个通道的数据，顺序同 `CHANNELS`。
/// `fill_row(y, row)` 给出第 `y` 行的像素（8 位、非预乘的 RGBA）。
fn encode_channels(rect: Rect, mut fill_row: impl FnMut(i32, &mut [u8])) -> io::Result<[Vec<u8>; 4]> {
    if rect.is_empty() {
        // 空图层：每个通道只有"压缩方式"这两个字节（0，不压缩），后面没有数据。
        return Ok(std::array::from_fn(|_| vec![0, 0]));
    }
    let (w, h) = ((rect.x1 - rect.x0) as usize, (rect.y1 - rect.y0) as usize);
    // 每个通道：压缩方式（1，RLE），各行压缩后的长度（每行两个字节），然后是各行的数据。
    let mut channels: [Vec<u8>; 4] = std::array::from_fn(|_| {
        let mut data = vec![0u8; 2 + h * 2];
        data[1] = 1;
        data
    });
    let mut rgba = vec![0u8; w * 4];
    let mut plane = vec![0u8; w];
    for (i, y) in (rect.y0..rect.y1).enumerate() {
        fill_row(y, &mut rgba);
        for (data, (_, offset)) in channels.iter_mut().zip(CHANNELS) {
            for (p, px) in plane.iter_mut().zip(rgba.as_chunks::<4>().0) {
                *p = px[offset];
            }
            pack_row(&plane, data, 2 + i * 2)?;
        }
    }
    Ok(channels)
}

/// 合并图像：压缩方式（1，RLE），三个通道所有行的长度，然后是红、绿、蓝三个平面的数据。
fn encode_merged(rgb: &[u8], w: usize, h: usize) -> io::Result<Vec<u8>> {
    let mut data = vec![0u8; 2 + 3 * h * 2];
    data[1] = 1;
    let mut plane = vec![0u8; w];
    for c in 0..3 {
        for (y, row) in rgb.chunks_exact(w * 3).enumerate() {
            for (p, px) in plane.iter_mut().zip(row.as_chunks::<3>().0) {
                *p = px[c];
            }
            pack_row(&plane, &mut data, 2 + (c * h + y) * 2)?;
        }
    }
    Ok(data)
}

/// 把一行压缩后接在 `data` 后面，并把压缩后的长度（两个字节）填到 `data` 的 `length_at` 处。
fn pack_row(plane: &[u8], data: &mut Vec<u8>, length_at: usize) -> io::Result<()> {
    let start = data.len();
    pack_bits(plane, data);
    // 画布最宽 16384，压缩后最多 16384 + 128 个字节，两个字节装得下；这个检查只是以防万一。
    let len = u16::try_from(data.len() - start)
        .map_err(|_| io::Error::other(tr("一行像素压缩后太长", "A row of pixels is too long after compression")))?;
    data[length_at..length_at + 2].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

// ---- 读取 ----

/// 文件不完整（读到一半没有了，或者某个长度超出了文件的范围）时的错误。
fn truncated() -> io::Error {
    invalid(tr("PSD 文件不完整或已损坏", "The PSD file is incomplete or damaged"))
}

/// 一段还没读的字节。所有的读取都从这里走，长度不够就报错，不会越界。
#[derive(Clone, Copy)]
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n).ok_or_else(truncated)?;
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>().ok_or_else(truncated)?;
        self.0 = rest;
        Ok(*head)
    }

    /// 接下来的 `n` 个字节单独作为一段：某一节的长度已知时，用它保证读这一节时不会读过头。
    fn sub(&mut self, n: usize) -> io::Result<Bytes<'a>> {
        self.take(n).map(Bytes)
    }

    fn u8(&mut self) -> io::Result<u8> {
        self.array().map(|[v]| v)
    }

    fn u16(&mut self) -> io::Result<u16> {
        self.array().map(u16::from_be_bytes)
    }

    fn i16(&mut self) -> io::Result<i16> {
        self.array().map(i16::from_be_bytes)
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.array().map(u32::from_be_bytes)
    }

    fn i32(&mut self) -> io::Result<i32> {
        self.array().map(i32::from_be_bytes)
    }

    /// 一个 32 位的长度。
    fn length(&mut self) -> io::Result<usize> {
        self.u32().map(|v| v as usize)
    }
}

/// 读 PSD 文件。返回文档，以及读入时被忽略或近似处理的内容的说明（给用户看的句子，按当前的界面语言写，
/// 没有则为空）。
/// 读出的文档没有撤销记录；最上面的图层是当前图层。
///
/// 文件打不开时原样返回系统的错误；文件内容不对或者是本程序读不了的种类时返回 `InvalidData`，附带说明。
/// 任何输入都不会让它崩溃。
///
/// 能读的：版本 1（不是 PSB）、RGB 模式、每通道 8 位或 16 位（16 位的直接换算成本程序的 15 位）、
/// 像素不压缩或者用 RLE 压缩。本程序没有的东西这样处理，并在说明里交代：
/// - 图层组读成文件夹（包括"穿透"），图层蒙版读成蒙版（包括矩形以外的颜色和停用），纯色填充图层读成填充图层；
/// - 矢量蒙版、蒙版的浓度和羽化、图层样式忽略；剪贴和"保护不透明度"两个开关照原样读入；
/// - 渐变、图案填充图层还画不出来：有存着的像素就显示它，没有就是透明的，存回 PSD 时原样不变；
/// - 文字图层、智能对象只留下文件里存着的像素；
/// - 调整图层跳过；文件里标明"像素与外观无关"的图层不读像素；
/// - 不认识的混合模式改成"正常"；
/// - 像素用 ZIP 压缩的图层读不了，跳过；
/// - 图层超出画布的部分裁掉；
/// - 文件里没有图层（或者一个能读的都没有）时，把合并图像读成一个图层。
pub fn load(path: &Path) -> io::Result<(Document, Vec<String>)> {
    // 整个文件读进内存再解析：各节的长度、各通道的数据都靠切片来取，越界就是报错，不会读到别处去。
    // 文件比解开之后的图层小得多，多占的这点内存不算什么。
    let mut file = File::open(path)?;
    let size = usize::try_from(file.metadata()?.len()).ok().and_then(|n| n.checked_add(1));
    let mut data = Vec::new();
    // 内存不够时报错，而不是让程序直接退出。
    size.and_then(|n| data.try_reserve_exact(n).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, tr("内存不足", "Out of memory")))?;
    file.read_to_end(&mut data)?;
    parse(&data)
}

fn parse(data: &[u8]) -> io::Result<(Document, Vec<String>)> {
    let mut notes = Notes::default();
    let mut r = Bytes(data);

    // 1. 文件头。
    if r.take(4).ok() != Some(SIGNATURE) {
        return Err(invalid(tr("不是 PSD 文件（开头不是 8BPS）", "Not a PSD file (it does not start with 8BPS)")));
    }
    match r.u16()? {
        1 => {}
        2 => {
            return Err(invalid(tr(
                "这是 PSB（大型文档格式）文件，本程序只能读普通的 PSD",
                "This is a PSB (Large Document Format) file; this program can only read regular PSD files",
            )))
        }
        v => {
            return Err(invalid(trf!(
                "PSD 的版本号是 {v}，本程序只认识版本 1",
                "The PSD version is {v}; this program only knows version 1"
            )))
        }
    }
    r.take(6)?;
    let channels = r.u16()? as usize;
    let (height, width) = (r.u32()?, r.u32()?);
    let (depth, mode) = (r.u16()?, r.u16()?);
    let sides = 1..=MAX_SIDE as u32;
    if !sides.contains(&width) || !sides.contains(&height) {
        return Err(invalid(trf!(
            "画布尺寸 {width}×{height} 超出了本程序支持的范围（每边 1 到 {MAX_SIDE}）",
            "Canvas size {width}×{height} is outside the range this program supports (1 to {MAX_SIDE} per side)"
        )));
    }
    if mode != MODE_RGB {
        let name = match mode {
            0 => tr("位图", "Bitmap").to_string(),
            1 => tr("灰度", "Grayscale").to_string(),
            2 => tr("索引颜色", "Indexed Color").to_string(),
            4 => "CMYK".to_string(),
            7 => tr("多通道", "Multichannel").to_string(),
            8 => tr("双色调", "Duotone").to_string(),
            9 => "Lab".to_string(),
            other => trf!("未知（{other}）", "unknown ({other})"),
        };
        return Err(invalid(trf!(
            "这个 PSD 的颜色模式是{name}，本程序只能读 RGB 模式的文件",
            "The color mode of this PSD is {name}; this program can only read RGB files"
        )));
    }
    let sample_bytes = match depth {
        8 => 1,
        16 => {
            notes.add(tr(
                "这个文件每通道 16 位，已按本程序的精度读入；再导出成 PSD 会是每通道 8 位",
                "This file has 16 bits per channel and was read at this program's precision; exporting it to PSD \
                 again gives 8 bits per channel",
            ));
            2
        }
        other => {
            return Err(invalid(trf!(
                "这个 PSD 每通道 {other} 位，本程序只能读 8 位和 16 位的",
                "This PSD has {other} bits per channel; this program can only read 8 or 16 bits"
            )))
        }
    };
    if !(3..=56).contains(&channels) {
        return Err(invalid(trf!(
            "通道数不对（{channels}）：RGB 模式至少要有 3 个通道",
            "Wrong number of channels ({channels}): RGB needs at least 3 channels"
        )));
    }
    let canvas = Rect { x0: 0, y0: 0, x1: width as i32, y1: height as i32 };
    let keep = Document::keep_area(width as i32, height as i32);

    // 2. 颜色模式数据，用不上。
    let n = r.length()?;
    r.take(n)?;
    // 3. 图像资源。
    let n = r.length()?;
    let (dpi, light) = read_resources(r.sub(n)?);
    let dpi = dpi.unwrap_or(DEFAULT_DPI);
    // 4. 图层与蒙版信息。
    let n = r.length()?;
    let records = read_layer_section(r.sub(n)?)?;

    let mut layers: Vec<Layer> = Vec::new();
    // 记录是自下而上排的：先是标出组的起点的那条隐藏记录，然后是组里的东西，最后才是组本身。
    // 所以遇到起点就深一层，遇到组本身就浅回来。不成对的由文档自己修正（`Document::from_layers`）。
    let mut depth: u16 = 0;
    for record in &records {
        let name = record.name.as_str();
        if record.section == SECTION_END {
            depth = depth.saturating_add(1);
            continue;
        }
        if record.adjustment {
            notes.layer(
                tr("下列调整图层本程序没有对应的功能，已跳过", "Skipped adjustment layers, which this program does not support"),
                name,
            );
            continue;
        }
        let mask = match &record.mask {
            Some(info) => {
                let mask = read_mask(record, info, sample_bytes, keep)
                    .map_err(|e| io::Error::new(e.kind(), trf!("图层“{name:.80}”的蒙版：{e}", "Mask of layer \"{name:.80}\": {e}")))?;
                if mask.is_none() {
                    notes.layer(tr("下列图层的蒙版是用 ZIP 压缩的，本程序读不了，已忽略", "Ignored ZIP-compressed masks of these layers"), name);
                }
                mask
            }
            None => None,
        };
        if record.vector_mask {
            notes.layer(tr("下列图层的矢量蒙版被忽略了", "Ignored the vector masks of these layers"), name);
        }
        if record.mask_params {
            notes.layer(tr("下列图层蒙版的浓度、羽化设置被忽略了", "Ignored the density and feather settings of these layer masks"), name);
        }
        let style = (!record.effects.is_empty()).then(|| {
            let (style, issues) = parse_style(&record.effects, light);
            let shown = record.section == 0 && record.fill.is_none_or(|(key, _)| key != *b"SoCo");
            if !shown {
                notes.layer(
                    tr(
                        "下列组和纯色填充图层的图层样式本程序不显示；存回 PSD 时原样保留",
                        "The layer styles of these groups and solid fill layers are not shown; they are kept as they are \
                         when saved to PSD",
                    ),
                    name,
                );
            }
            for issue in issues.into_iter().filter(|_| shown) {
                notes.layer(issue.text(), name);
            }
            style
        });

        if matches!(record.section, SECTION_OPEN | SECTION_CLOSED) {
            // 组本身。混合模式以分隔块里写的为准，"穿透"就是穿透。
            depth = depth.saturating_sub(1);
            let key = record.section_blend.unwrap_or(record.blend);
            let pass_through = &key == PASS_THROUGH;
            let mut folder = Layer::folder(name);
            folder.kind = Kind::Folder { open: record.section == SECTION_OPEN };
            folder.depth = depth;
            folder.props = LayerProps {
                name: name.to_string(),
                opacity: record.opacity as f32 / 255.0,
                visible: !record.hidden,
                blend: if pass_through { BlendMode::Normal } else { layer_blend(&key, name, &mut notes) },
                lock_alpha: record.keep_alpha,
                clip: record.clipping,
                pass_through,
                ..Default::default()
            };
            folder.mask = mask;
            folder.style = style;
            layers.push(folder);
            continue;
        }

        // 标明"像素与外观无关"的图层，文件里即使存着像素也不能用（规范说那是未定义的），当作没有。
        // 填充图层除外：它存着的像素就是画好的样子，渐变、图案要靠它显示。
        let rect = if record.irrelevant && record.fill.is_none() { Rect::EMPTY } else { record.rect };
        let mut tiles = TileMap::default();
        if !rect.is_empty() {
            let outcome = read_layer_pixels(record, sample_bytes, keep, &mut tiles)
                .map_err(|e| io::Error::new(e.kind(), trf!("图层“{name:.80}”：{e}", "Layer \"{name:.80}\": {e}")))?;
            if let PixelsRead::Zip = outcome {
                notes.layer(
                    tr(
                        "下列图层的像素是用 ZIP 压缩的，本程序读不了，已跳过",
                        "Skipped layers with ZIP-compressed pixels, which this program cannot read",
                    ),
                    name,
                );
                continue;
            }
            if rect.intersect(keep) != rect {
                notes.layer(shared::clipped_kind(), name);
            }
        } else if !record.rect.is_empty() && record.fill.is_none() {
            notes.layer(
                tr(
                    "下列图层的像素在文件里标明了与画面无关，没有读入",
                    "Did not read the pixels of these layers, which the file marks as not affecting the image",
                ),
                name,
            );
        }
        if record.text {
            notes.layer(
                tr(
                    "下列文字图层只读入了像素，文字不能再编辑",
                    "Read only the pixels of these text layers; the text can no longer be edited",
                ),
                name,
            );
        }
        if record.smart {
            notes.layer(tr("下列智能对象只读入了像素", "Read only the pixels of these smart objects"), name);
        }
        let props = LayerProps {
            name: name.to_string(),
            opacity: record.opacity as f32 / 255.0,
            visible: !record.hidden,
            blend: layer_blend(&record.blend, name, &mut notes),
            lock_alpha: record.keep_alpha,
            clip: record.clipping,
            ..Default::default()
        };
        let mut layer = Layer::pixels(props, TileMap::default());
        match record.fill {
            Some((key, data)) => {
                // 填充图层：纯色的读出颜色；渐变、图案还画不出来，有存着的像素就显示它。
                let paint = match &key {
                    b"SoCo" => solid_color(data).map(Paint::Solid),
                    _ => None,
                };
                let known = paint.is_some();
                if !known {
                    notes.layer(
                        if tiles.is_empty() {
                            tr(
                                "下列填充图层（渐变、图案，或者读不懂的纯色）本程序还画不出来，显示成透明；存回 PSD 时原样保留",
                                "Cannot draw these fill layers (gradient, pattern, or an unreadable solid color) yet; they show as \
                                 transparent and are kept as they are when saved to PSD",
                            )
                        } else {
                            tr(
                                "下列填充图层（渐变、图案，或者读不懂的纯色）显示的是文件里存着的像素；存回 PSD 时原样保留",
                                "These fill layers (gradient, pattern, or an unreadable solid color) show the pixels stored in the \
                                 file; they are kept as they are when saved to PSD",
                            )
                        },
                        name,
                    );
                }
                layer.kind = Kind::Fill(Fill { paint: paint.unwrap_or(Paint::Other), psd: Some((key, data.to_vec())) });
                if !known {
                    layer.tiles = tiles;
                }
            }
            None => layer.tiles = tiles,
        }
        layer.depth = depth;
        layer.mask = mask;
        layer.style = style;
        layers.push(layer);
    }
    for (i, layer) in layers.iter_mut().enumerate() {
        if layer.props.name.is_empty() {
            let n = i + 1;
            layer.props.name = trf!("图层 {n}", "Layer {n}");
        }
    }

    // 5. 合并图像。只有一个图层也读不出来的时候才用它。
    if layers.is_empty() {
        if !records.is_empty() {
            notes.add(tr(
                "文件里没有本程序能读的图层，读入的是合并之后的图像",
                "The file has no layers this program can read; loaded the merged image instead",
            ));
        }
        let tiles = read_merged(r, channels, sample_bytes, canvas, &mut notes)
            .map_err(|e| io::Error::new(e.kind(), trf!("合并图像：{e}", "Merged image: {e}")))?;
        let props = LayerProps { name: tr("图层 1", "Layer 1").into(), ..Default::default() };
        layers.push(Layer::pixels(props, tiles));
    }

    let background = take_background(&mut layers, canvas).unwrap_or([255; 3]);
    let mut doc = Document::from_layers(canvas.x1, canvas.y1, background, layers);
    doc.dpi = dpi;
    Ok((doc, notes.into_vec()))
}

/// 图层记录里的混合键对应的混合模式；本程序没有的改成"正常"，并在说明里交代。
fn layer_blend(key: &[u8; 4], name: &str, notes: &mut Notes) -> BlendMode {
    blend_of(key).unwrap_or_else(|| {
        let text = String::from_utf8_lossy(key);
        let text = text.trim_end();
        notes.layer(shared::unknown_blend_kind(), &trf!("{name}（{text}）", "{name} ({text})"));
        BlendMode::Normal
    })
}

/// 在图像资源里找分辨率。资源块的结构：签名、编号、名字（Pascal 字符串，补成偶数长度）、
/// 数据长度、数据（补成偶数长度）。找不到或者资源坏了就返回 `None`，这只是附带的信息。
/// 图像资源里用得上的：分辨率，全局光（角度、高度）。
fn read_resources(mut r: Bytes) -> (Option<u32>, Option<(i32, i32)>) {
    let (mut dpi, mut angle, mut altitude) = (None, None, None);
    let mut step = |r: &mut Bytes| -> Option<()> {
        r.take(4).ok()?;
        let id = r.u16().ok()?;
        let name_len = r.u8().ok()? as usize;
        r.take(name_len + (name_len + 1) % 2).ok()?;
        let len = r.length().ok()?;
        let mut data = r.sub(len).ok()?;
        if len % 2 == 1 {
            let _ = r.take(1);
        }
        match id {
            RESOURCE_RESOLUTION => {
                // 横向分辨率，16.16 定点数。不管显示单位选的是什么，存的都是像素/英寸。
                let d = (data.u32().ok()? as u64 + 0x8000) >> 16;
                dpi = (d >= 1).then_some(d as u32);
            }
            RESOURCE_GLOBAL_ANGLE => angle = data.i32().ok(),
            RESOURCE_GLOBAL_ALTITUDE => altitude = data.i32().ok(),
            _ => {}
        }
        Some(())
    };
    while r.len() > 0 {
        if step(&mut r).is_none() {
            break;
        }
    }
    (dpi, angle.map(|a| (a, altitude.unwrap_or(30))))
}

/// 一条图层记录里用得上的内容。
struct Record<'a> {
    /// 图层的像素在画布上占的矩形。已经检查过：边长不是负的，也没有大得离谱
    rect: Rect,
    channels: Vec<Channel<'a>>,
    blend: [u8; 4],
    opacity: u8,
    clipping: bool,
    hidden: bool,
    keep_alpha: bool,
    /// 文件里标明了这个图层的像素与文档的外观无关
    irrelevant: bool,
    name: String,
    /// 图层组分隔记录的类型（`SECTION_*`），普通图层是 0
    section: u32,
    /// 组的分隔块里写的混合键
    section_blend: Option<[u8; 4]>,
    /// 图层蒙版
    mask: Option<MaskInfo>,
    /// 有矢量蒙版
    vector_mask: bool,
    /// 蒙版带着浓度、羽化这些参数
    mask_params: bool,
    /// 有图层样式
    /// 图层样式的块（`lfx2`、旧版的 `lrFX`、`lmfx`）
    effects: Vec<([u8; 4], &'a [u8])>,
    /// 文字图层
    text: bool,
    /// 智能对象
    smart: bool,
    /// 调整图层
    adjustment: bool,
    /// 填充图层（纯色、渐变、图案）的描述符块：键和数据
    fill: Option<([u8; 4], &'a [u8])>,
}

/// 蒙版数据里用得上的内容。
struct MaskInfo {
    rect: Rect,
    /// 矩形以外的颜色，0 或 255
    default: u8,
    disabled: bool,
    /// 蒙版的像素在哪个通道
    channel: i16,
}

struct Channel<'a> {
    /// −1 透明度，0、1、2 红绿蓝，−2、−3 蒙版
    id: i16,
    len: usize,
    /// 这个通道的图像数据，开头两个字节是压缩方式
    data: &'a [u8],
}

/// 读"图层与蒙版信息"这一节，返回各图层的记录（自下而上）。没有图层就是空的。
fn read_layer_section(mut section: Bytes) -> io::Result<Vec<Record>> {
    if section.len() < 4 {
        return Ok(Vec::new());
    }
    let n = section.length()?;
    let info = section.sub(n)?;
    let mut records = if info.len() >= 2 { read_layer_info(info)? } else { Vec::new() };
    // 后面是全局图层蒙版信息（用不上），再后面是一串附加信息块。每通道 16 位的文件，
    // 上面的"图层信息"是空的，真正的图层信息放在 `Lr16` 块里，内容的结构一样。
    let Ok(n) = section.length() else { return Ok(records) };
    if section.take(n).is_err() {
        return Ok(records);
    }
    for (key, data) in Blocks(section) {
        if records.is_empty() && matches!(&key, b"Lr16" | b"Lr32" | b"Layr") {
            records = read_layer_info(Bytes(data))?;
        }
    }
    Ok(records)
}

/// "图层信息"：图层个数、各图层的记录，然后是各图层各通道的图像数据。
fn read_layer_info(mut r: Bytes) -> io::Result<Vec<Record>> {
    // 个数是负数表示合并图像的第一个附加通道是透明度，图层的个数是它的绝对值。
    let count = r.i16()?.unsigned_abs() as usize;
    if count > MAX_LAYERS {
        return Err(invalid(trf!(
            "图层个数不正常（{count}），文件可能已损坏",
            "Unusual number of layers ({count}); the file may be damaged"
        )));
    }
    let mut records = Vec::with_capacity(count);
    for index in 0..count {
        records.push(read_record(&mut r, index)?);
    }
    for record in &mut records {
        let name = &record.name;
        for channel in &mut record.channels {
            channel.data = r.take(channel.len).map_err(|_| {
                invalid(trf!(
                    "图层“{name:.80}”的通道数据长度不对，超出了文件的范围",
                    "The channel data length of layer \"{name:.80}\" is wrong and goes past the end of the file"
                ))
            })?;
        }
    }
    Ok(records)
}

fn read_record<'a>(r: &mut Bytes<'a>, index: usize) -> io::Result<Record<'a>> {
    let n = index + 1;
    let bad = |what: &str| invalid(trf!("第 {n} 个图层的记录有问题：{what}", "The record of layer {n} is invalid: {what}"));
    let (top, left, bottom, right) = (r.i32()?, r.i32()?, r.i32()?, r.i32()?);
    let (w, h) = (right as i64 - left as i64, bottom as i64 - top as i64);
    if !(0..=MAX_LAYER_SIDE).contains(&w) || !(0..=MAX_LAYER_SIDE).contains(&h) {
        return Err(bad(&trf!(
            "矩形不正常（上 {top}、左 {left}、下 {bottom}、右 {right}）",
            "unusual rectangle (top {top}, left {left}, bottom {bottom}, right {right})"
        )));
    }
    let count = r.u16()? as usize;
    if count > MAX_CHANNELS {
        return Err(bad(&trf!("通道太多（{count} 个）", "too many channels ({count})")));
    }
    let mut channels = Vec::with_capacity(count);
    for _ in 0..count {
        channels.push(Channel { id: r.i16()?, len: r.length()?, data: &[] });
    }
    if r.take(4)? != BLOCK_SIGNATURE {
        return Err(bad(tr("混合模式前面的签名不对", "wrong signature before the blend mode")));
    }
    let blend = r.array()?;
    let opacity = r.u8()?;
    let clipping = r.u8()? != 0;
    let flags = r.u8()?;
    r.u8()?;

    // 后面是长度可变的附加数据：蒙版、混合范围、名字，然后是若干附加信息块。
    let n = r.length()?;
    let mut extra = r.sub(n)?;
    let n = extra.length()?;
    let (mask, mask_params) = read_mask_info(extra.sub(n)?, &channels).map_err(|what| bad(&what))?;
    let n = extra.length()?;
    extra.take(n)?;
    // 名字是 Pascal 字符串，连长度字节一起补到 4 的倍数。编码没有定准，先当 UTF-8 读；
    // 后面有 Unicode 名字（`luni` 块）的话以它为准。
    let n = extra.u8()? as usize;
    let mut name = String::from_utf8_lossy(extra.take(n)?).into_owned();
    let padding = (n + 1).next_multiple_of(4) - (n + 1);
    extra.take(padding.min(extra.len()))?;

    let mut record = Record {
        rect: Rect { x0: left, y0: top, x1: right, y1: bottom },
        channels,
        blend,
        opacity,
        clipping,
        hidden: flags & FLAG_HIDDEN != 0,
        keep_alpha: flags & FLAG_KEEP_ALPHA != 0,
        irrelevant: flags & (FLAG_BIT4_VALID | FLAG_IRRELEVANT) == FLAG_BIT4_VALID | FLAG_IRRELEVANT,
        name: String::new(),
        section: 0,
        section_blend: None,
        mask,
        vector_mask: false,
        mask_params,
        effects: Vec::new(),
        text: false,
        smart: false,
        adjustment: false,
        fill: None,
    };
    for (key, data) in Blocks(extra) {
        match &key {
            b"luni" => {
                if let Some(unicode) = read_unicode_name(Bytes(data)).filter(|n| !n.is_empty()) {
                    name = unicode;
                }
            }
            // 图层组的分隔记录。嵌套很深的组用的是 `lsdk`，内容一样。
            b"lsct" | b"lsdk" => {
                record.section = Bytes(data).u32().unwrap_or(0);
                if data.len() >= 12 && &data[4..8] == BLOCK_SIGNATURE {
                    record.section_blend = data[8..12].try_into().ok();
                }
            }
            b"vmsk" | b"vsms" => record.vector_mask = true,
            b"lrFX" | b"lfx2" | b"lmfx" => record.effects.push((key, data)),
            b"TySh" | b"tySh" => record.text = true,
            b"SoLd" | b"SoLE" | b"PlLd" | b"plLd" => record.smart = true,
            b"SoCo" | b"GdFl" | b"PtFl" => record.fill = Some((key, data)),
            // 各种调整图层：亮度/对比度、色阶、曲线、曝光度、自然饱和度、色相/饱和度、色彩平衡、黑白、
            // 照片滤镜、通道混合器、颜色查找、反相、色调分离、阈值、渐变映射、可选颜色。
            b"brit" | b"CgEd" | b"levl" | b"curv" | b"expA" | b"vibA" | b"hue " | b"hue2" | b"blnc" | b"blwh" | b"phfl"
            | b"mixr" | b"clrL" | b"nvrt" | b"post" | b"thrs" | b"grdm" | b"selc" => record.adjustment = true,
            _ => {}
        }
    }
    record.name = name;
    Ok(record)
}

/// 图层记录里的蒙版数据：矩形、矩形以外的颜色、标志；有参数（浓度、羽化）时跟着参数；
/// 同时有矢量蒙版和图层蒙版时，后面还有一套"真正的"标志、颜色和矩形，说的是 −3 号通道里的图层蒙版。
/// 返回蒙版（没有像素蒙版时是 None）和它是不是带着参数。数据不对时返回说明。
fn read_mask_info(mut m: Bytes, channels: &[Channel]) -> Result<(Option<MaskInfo>, bool), String> {
    if m.len() < 18 {
        return Ok((None, false));
    }
    let short = || tr("蒙版数据不完整", "incomplete mask data").to_string();
    let rect = |m: &mut Bytes| -> Result<Rect, String> {
        let (top, left, bottom, right) = (m.i32().map_err(|_| short())?, m.i32().map_err(|_| short())?, m.i32().map_err(|_| short())?, m.i32().map_err(|_| short())?);
        let (w, h) = (right as i64 - left as i64, bottom as i64 - top as i64);
        if !(0..=MAX_LAYER_SIDE).contains(&w) || !(0..=MAX_LAYER_SIDE).contains(&h) {
            return Err(trf!("蒙版的矩形不正常（上 {top}、左 {left}、下 {bottom}、右 {right}）", "unusual mask rectangle (top {top}, left {left}, bottom {bottom}, right {right})"));
        }
        Ok(Rect { x0: left, y0: top, x1: right, y1: bottom })
    };
    let first = rect(&mut m)?;
    let default = m.u8().map_err(|_| short())?;
    let flags = m.u8().map_err(|_| short())?;
    let mut params = false;
    if flags & MASK_HAS_PARAMS != 0 {
        let which = m.u8().unwrap_or(0);
        params = which != 0;
        // 依次是图层蒙版的浓度（1 字节）、羽化（8 字节）、矢量蒙版的浓度、羽化。用不上，跳过。
        for (bit, size) in [(1, 1), (2, 8), (4, 1), (8, 8)] {
            if which & bit != 0 {
                let _ = m.take(size.min(m.len()));
            }
        }
    }
    let has = |id: i16| channels.iter().any(|c| c.id == id);
    if has(REAL_MASK_CHANNEL) && m.len() >= 18 {
        let real_flags = m.u8().map_err(|_| short())?;
        let real_default = m.u8().map_err(|_| short())?;
        let real = rect(&mut m)?;
        return Ok((Some(MaskInfo { rect: real, default: real_default, disabled: real_flags & MASK_DISABLED != 0, channel: REAL_MASK_CHANNEL }), params));
    }
    if has(MASK_CHANNEL) {
        return Ok((Some(MaskInfo { rect: first, default, disabled: flags & MASK_DISABLED != 0, channel: MASK_CHANNEL }), params));
    }
    // 只有矢量蒙版：前面那套说的是矢量蒙版，没有像素蒙版。
    Ok((None, params))
}

/// 描述符（PSD 里"动作描述符"那种结构，填充图层、图层样式的参数就存成这个）里本程序用得上的值。
enum Desc {
    Num(f64),
    /// 真假，和那个字节在内存里的位置（改总开关时要找到它）
    Bool(bool, usize),
    /// 枚举的值（类型读过去了，用不上）
    Enum(Vec<u8>),
    List(Vec<Desc>),
    /// 对象：类名和各项
    Obj(Vec<u8>, Vec<(Vec<u8>, Desc)>),
    /// 别的种类，读过去了，但用不上
    Other,
}

impl Desc {
    fn get(&self, key: &[u8]) -> Option<&Desc> {
        match self {
            Desc::Obj(_, items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn num(&self, key: &[u8]) -> Option<f64> {
        match self.get(key)? {
            Desc::Num(n) if n.is_finite() => Some(*n),
            _ => None,
        }
    }

    fn flag(&self, key: &[u8]) -> Option<bool> {
        match self.get(key)? {
            Desc::Bool(b, _) => Some(*b),
            _ => None,
        }
    }

    fn word(&self, key: &[u8]) -> Option<&[u8]> {
        match self.get(key)? {
            Desc::Enum(v) => Some(v),
            _ => None,
        }
    }
}

/// 读一个描述符：名字（Unicode 字符串）、类名、项数、各项（键、类型、值），得到一个 `Desc::Obj`。
/// 读不下去时返回 None。
fn read_descriptor(r: &mut Bytes, level: usize) -> Option<Desc> {
    let n = r.length().ok()?;
    r.take(n.checked_mul(2)?).ok()?;
    let class = read_id(r)?;
    let count = r.length().ok()?;
    if count > 10_000 {
        return None;
    }
    let mut items = Vec::new();
    for _ in 0..count {
        let key = read_id(r)?;
        let kind: [u8; 4] = r.array().ok()?;
        items.push((key, read_desc_value(r, &kind, level)?));
    }
    Some(Desc::Obj(class, items))
}

/// 键或类名：四个字节的长度，为 0 时后面是四个字符的编号，否则是这么长的字符串。
fn read_id(r: &mut Bytes) -> Option<Vec<u8>> {
    let n = r.length().ok()?;
    Some(r.take(if n == 0 { 4 } else { n }).ok()?.to_vec())
}

fn read_desc_value(r: &mut Bytes, kind: &[u8; 4], level: usize) -> Option<Desc> {
    if level > 32 {
        return None;
    }
    let float = |r: &mut Bytes| -> Option<f64> { Some(f64::from_bits(u64::from_be_bytes(r.array().ok()?))) };
    Some(match kind {
        b"Objc" | b"GlbO" => read_descriptor(r, level + 1)?,
        b"doub" => Desc::Num(float(r)?),
        b"UntF" => {
            r.take(4).ok()?;
            Desc::Num(float(r)?)
        }
        b"long" => Desc::Num(r.i32().ok()? as f64),
        b"comp" => {
            r.take(8).ok()?;
            Desc::Other
        }
        b"bool" => {
            let at = r.0.as_ptr() as usize;
            Desc::Bool(r.u8().ok()? != 0, at)
        }
        b"TEXT" => {
            let n = r.length().ok()?;
            r.take(n.checked_mul(2)?).ok()?;
            Desc::Other
        }
        b"enum" => {
            read_id(r)?;
            Desc::Enum(read_id(r)?)
        }
        b"type" | b"GlbC" => {
            let n = r.length().ok()?;
            r.take(n.checked_mul(2)?).ok()?;
            read_id(r)?;
            Desc::Other
        }
        b"VlLs" => {
            let n = r.length().ok()?;
            let mut items = Vec::new();
            for _ in 0..n.min(100_000) {
                let kind: [u8; 4] = r.array().ok()?;
                items.push(read_desc_value(r, &kind, level + 1)?);
            }
            Desc::List(items)
        }
        b"alis" | b"tdta" => {
            let n = r.length().ok()?;
            r.take(n).ok()?;
            Desc::Other
        }
        b"UnFl" => {
            r.take(4).ok()?;
            let n = r.length().ok()?;
            r.take(n.checked_mul(8)?).ok()?;
            Desc::Other
        }
        _ => return None,
    })
}

/// 读图层样式时遇到的、要在说明里交代的事。
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum StyleIssue {
    /// 有本程序画不出来的效果
    Unsupported,
    /// 投影、发光用了本程序没有的设置，按近似画
    Approximate,
    /// 整个读不懂
    Unreadable,
}

impl StyleIssue {
    fn text(self) -> &'static str {
        match self {
            StyleIssue::Unsupported => tr(
                "下列图层样式里有本程序画不出来的效果（斜面和浮雕、描边、内阴影、光泽、颜色/渐变/图案叠加），这些效果不显示；存回 PSD 时样式原样保留",
                "These layer styles have effects this program cannot draw (bevel, stroke, inner shadow, satin, overlays); \
                 those effects are not shown, and the styles are kept as they are when saved to PSD",
            ),
            StyleIssue::Approximate => tr(
                "下列图层样式里的投影或发光用了本程序没有的设置（非直线的等高线、杂色、精确手法、别的混合模式），按近似画",
                "Drop shadows or glows in these layer styles use settings this program does not have (contours, noise, \
                 the precise technique, other blend modes); they are drawn approximately",
            ),
            StyleIssue::Unreadable => tr(
                "下列图层的图层样式读不懂，不显示；存回 PSD 时原样保留",
                "Could not read the layer styles of these layers; they are not shown and are kept as they are when saved to PSD",
            ),
        }
    }
}

/// 从图层样式的块（`lfx2` 等）读出本程序会画的效果（实现方案 §3.14）。`light` 是文件的全局光。
/// 读不懂的照样留着原始的块，存回 PSD 时写回去。
pub(crate) fn parse_style(blocks: &[([u8; 4], &[u8])], light: Option<(i32, i32)>) -> (Style, Vec<StyleIssue>) {
    let psd: Vec<([u8; 4], Vec<u8>)> = blocks.iter().map(|(k, d)| (*k, d.to_vec())).collect();
    let mut style = Style { psd, effects: Vec::new(), enabled: true, switch_at: None, light };
    let Some(bi) = style.psd.iter().position(|(k, _)| k == b"lfx2") else { return (style, vec![StyleIssue::Unreadable]) };
    let data = style.psd[bi].1.clone();
    let base = data.as_ptr() as usize;
    let mut r = Bytes(&data);
    let parsed = (|| {
        r.u32().ok()?;
        if r.u32().ok()? != 16 {
            return None;
        }
        read_descriptor(&mut r, 0)
    })();
    let Some(root @ Desc::Obj(..)) = parsed else { return (style, vec![StyleIssue::Unreadable]) };
    let Desc::Obj(_, items) = &root else { unreachable!() };
    let mut issues = Vec::new();
    let scale = root.num(b"Scl ").map_or(1.0, |v| v / 100.0).clamp(0.01, 10.0);
    // 全局光的角度：文件里没有的话是 PS 默认的 120°。
    let global = light.map_or(120.0, |(a, _)| a as f64);
    if let Some(Desc::Bool(on, at)) = root.get(b"masterFXSwitch") {
        style.enabled = *on;
        style.switch_at = Some((bi, at - base));
    }
    let mut effects = Vec::new();
    for (key, value) in items {
        // 新版的 PS 一种效果可以有好几个，存成列表（键名以 Multi 结尾）。
        let list: Vec<&Desc> = match value {
            Desc::List(v) => v.iter().collect(),
            v @ Desc::Obj(..) => vec![v],
            _ => continue,
        };
        for fx in list.into_iter().filter(|fx| fx.flag(b"enab").unwrap_or(false) && fx.flag(b"present").unwrap_or(true)) {
            let kind = match key.as_slice() {
                b"DrSh" | b"dropShadowMulti" => {
                    let angle = if fx.flag(b"uglg").unwrap_or(false) { global } else { fx.num(b"lagl").unwrap_or(global) };
                    let d = fx.num(b"Dstn").unwrap_or(0.0) * scale;
                    let (dx, dy) = (-angle.to_radians().cos() * d, angle.to_radians().sin() * d);
                    EffectKind::DropShadow { dx: dx as f32, dy: dy as f32, knockout: fx.flag(b"layerConceals").unwrap_or(true) }
                }
                b"OrGl" => EffectKind::OuterGlow,
                b"IrGl" => EffectKind::InnerGlow { center: fx.word(b"glwS") == Some(b"SrcC") },
                b"IrSh" | b"innerShadowMulti" | b"ebbl" | b"ChFX" | b"SoFi" | b"solidFillMulti" | b"GrFl" | b"gradientFillMulti"
                | b"patternFill" | b"FrFX" | b"frameFXMulti" => {
                    if !issues.contains(&StyleIssue::Unsupported) {
                        issues.push(StyleIssue::Unsupported);
                    }
                    continue;
                }
                _ => continue,
            };
            let blend = match fx.word(b"Md  ").unwrap_or(b"Nrml") {
                b"Nrml" => Some(BlendMode::Normal),
                b"Mltp" => Some(BlendMode::Multiply),
                b"Scrn" => Some(BlendMode::Screen),
                b"Ovrl" => Some(BlendMode::Overlay),
                b"Drkn" => Some(BlendMode::Darken),
                b"Lghn" => Some(BlendMode::Lighten),
                b"Dfrn" => Some(BlendMode::Difference),
                b"linearDodge" => Some(BlendMode::Add),
                _ => None,
            };
            // 等高线不是从 (0, 0) 到 (255, 255) 的直线、带杂色、用"精确"手法的，都只能近似。
            let linear = match fx.get(b"TrnS").and_then(|c| c.get(b"Crv ")) {
                None => true,
                Some(Desc::List(points)) => {
                    let p: Vec<(f64, f64)> = points.iter().filter_map(|p| Some((p.num(b"Hrzn")?, p.num(b"Vrtc")?))).collect();
                    p == [(0.0, 0.0), (255.0, 255.0)]
                }
                Some(_) => false,
            };
            let rough = blend.is_none() || !linear || fx.num(b"Nose").unwrap_or(0.0) > 0.0 || fx.word(b"GlwT") == Some(b"PrBL");
            if rough && !issues.contains(&StyleIssue::Approximate) {
                issues.push(StyleIssue::Approximate);
            }
            effects.push(Effect {
                kind,
                color: fx.get(b"Clr ").and_then(desc_color).unwrap_or([0; 3]),
                opacity: (fx.num(b"Opct").unwrap_or(75.0) / 100.0).clamp(0.0, 1.0) as f32,
                blend: blend.unwrap_or(BlendMode::Normal),
                spread: (fx.num(b"Ckmt").unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32,
                size: (fx.num(b"blur").unwrap_or(0.0) * scale).clamp(0.0, 2000.0) as f32,
                range: (fx.num(b"Inpr").unwrap_or(50.0) / 100.0).clamp(0.01, 1.0) as f32,
            });
        }
    }
    style.effects = effects;
    (style, issues)
}

/// 纯色填充图层（`SoCo` 块）的颜色。RGB、HSB、灰度、CMYK、Lab 几种颜色都换成 sRGB；读不出来时返回 None。
fn solid_color(data: &[u8]) -> Option<[u8; 3]> {
    let mut r = Bytes(data);
    if r.u32().ok()? != 16 {
        return None;
    }
    let Desc::Obj(_, items) = read_descriptor(&mut r, 0)? else { return None };
    desc_color(items.iter().find(|(k, _)| k == b"Clr ").map(|(_, v)| v)?)
}

/// 描述符里一个颜色对象换成 sRGB：RGB、HSB、灰度、CMYK、Lab 几种。读不出来时返回 None。
fn desc_color(v: &Desc) -> Option<[u8; 3]> {
    let Desc::Obj(class, color) = v else { return None };
    let num = |key: &[u8]| {
        color.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
            Desc::Num(n) => Some(*n),
            _ => None,
        })
    };
    let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    match class.as_slice() {
        b"RGBC" => Some([num(b"Rd  ")?, num(b"Grn ")?, num(b"Bl  ")?].map(|v| byte(v / 255.0))),
        b"HSBC" => {
            let (h, sat, v) = (num(b"H   ")?.rem_euclid(360.0) / 60.0, num(b"Strt")? / 100.0, num(b"Brgh")? / 100.0);
            let (i, f) = (h.floor(), h.fract());
            let (p, q, t) = (v * (1.0 - sat), v * (1.0 - sat * f), v * (1.0 - sat * (1.0 - f)));
            let rgb = match i as i32 {
                0 => [v, t, p],
                1 => [q, v, p],
                2 => [p, v, t],
                3 => [p, q, v],
                4 => [t, p, v],
                _ => [v, p, q],
            };
            Some(rgb.map(byte))
        }
        // 灰度存的是"黑"的百分比：0 是白。
        b"Grsc" => Some([byte(1.0 - num(b"Gry ")? / 100.0); 3]),
        // 不经过色彩管理的简单换算，只求大致对。
        b"CMYC" => {
            let k = 1.0 - num(b"Blck")? / 100.0;
            Some([num(b"Cyn ")?, num(b"Mgnt")?, num(b"Ylw ")?].map(|c| byte((1.0 - c / 100.0) * k)))
        }
        b"LbCl" => Some(lab_to_srgb(num(b"Lmnc")?, num(b"A   ")?, num(b"B   ")?)),
        _ => None,
    }
}

/// Lab（D50 白点，Photoshop 用的）换成 sRGB：先到 XYZ，再用 Bradford 适配过的矩阵到线性 sRGB，最后加 sRGB 的伽马。
fn lab_to_srgb(l: f64, a: f64, b: f64) -> [u8; 3] {
    let fy = (l + 16.0) / 116.0;
    let (fx, fz) = (fy + a / 500.0, fy - b / 200.0);
    let inv = |t: f64| if t > 6.0 / 29.0 { t * t * t } else { 3.0 * (6.0f64 / 29.0).powi(2) * (t - 4.0 / 29.0) };
    let (x, y, z) = (0.9642 * inv(fx), inv(fy), 0.8251 * inv(fz));
    let lin = [
        3.1338561 * x - 1.6168667 * y - 0.4906146 * z,
        -0.9787684 * x + 1.9161415 * y + 0.0334540 * z,
        0.0719453 * x - 0.2289914 * y + 1.4052427 * z,
    ];
    lin.map(|c| {
        let c = c.clamp(0.0, 1.0);
        let g = if c <= 0.0031308 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
        (g * 255.0).round() as u8
    })
}

/// `luni` 块里的图层名：四个字节的字符数（UTF-16 码元的个数），然后是 UTF-16 的字符。
fn read_unicode_name(mut r: Bytes) -> Option<String> {
    let count = r.length().ok()?;
    // 字符数写得比实际的数据还多的，有多少读多少。
    let bytes = r.take(count.saturating_mul(2).min(r.len() / 2 * 2)).ok()?;
    let units = bytes.as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b));
    let name: String = char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect();
    // 有的程序把结尾的 0 也算在字符数里。
    Some(name.trim_end_matches('\0').to_string())
}

/// 依次取出一串附加信息块：签名（`8BIM` 或 `8B64`）、四个字符的键、数据长度、数据。
struct Blocks<'a>(Bytes<'a>);

impl<'a> Iterator for Blocks<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let r = &mut self.0;
        // 规范说数据要补成偶数长度，实际的文件有补到 4 的倍数的，也有长度是奇数却不补的。
        // 所以不看上一块的长度怎么写，直接往后找签名，最多跳过 3 个字节。
        let mut skipped = 0;
        while !matches!(r.0.first_chunk::<4>()?, b"8BIM" | b"8B64") {
            if skipped == 3 {
                return None;
            }
            r.0 = &r.0[1..];
            skipped += 1;
        }
        r.take(4).ok()?;
        let key = r.array().ok()?;
        let len = r.length().ok()?;
        // 长度写得超出范围的（有的程序写的 `Lr16` 块就是这样），有多少算多少。
        let data = r.take(len.min(r.len())).ok()?;
        Some((key, data))
    }
}

/// 一个通道（一个平面）的图像数据，一行一行往下读。
enum Plane<'a> {
    /// 不压缩：一行接着一行
    Raw(&'a [u8]),
    /// RLE：`lens` 是各行压缩后的长度（每行两个字节），`data` 是各行的数据
    Rle { lens: &'a [u8], data: &'a [u8] },
    /// 文件里没有这个通道（比如没有透明度通道的图层）：每个字节都是这个值
    Fill(u8),
}

impl<'a> Plane<'a> {
    /// `data` 是一个通道的数据，开头两个字节是压缩方式；解开之后应当有 `rows` 行，每行 `row_bytes` 个字节。
    /// 用 ZIP 压缩的读不了，返回 `None`。
    fn open(data: &'a [u8], row_bytes: usize, rows: usize) -> io::Result<Option<Self>> {
        let bad = || invalid(tr("通道数据的长度和图层的尺寸对不上", "channel data length does not match the layer size"));
        let (compression, body) = data.split_first_chunk::<2>().ok_or_else(bad)?;
        match u16::from_be_bytes(*compression) {
            0 => {
                let need = row_bytes.checked_mul(rows).ok_or_else(bad)?;
                Ok(Some(Plane::Raw(body.get(..need).ok_or_else(bad)?)))
            }
            1 => {
                let (lens, data) = body.split_at_checked(rows.checked_mul(2).ok_or_else(bad)?).ok_or_else(bad)?;
                Ok(Some(Plane::Rle { lens, data }))
            }
            2 | 3 => Ok(None),
            other => Err(invalid(unknown_compression(other))),
        }
    }

    /// 读下一行，放进 `out`（长度是一行的字节数）；`out` 是 `None` 就只是跳过这一行。
    fn next_row(&mut self, row_bytes: usize, out: Option<&mut [u8]>) -> io::Result<()> {
        match self {
            Plane::Raw(data) => {
                let (row, rest) = data.split_at_checked(row_bytes).ok_or_else(truncated)?;
                *data = rest;
                if let Some(out) = out {
                    out.copy_from_slice(row);
                }
            }
            Plane::Rle { lens, data } => {
                let (len, rest) = lens.split_first_chunk::<2>().ok_or_else(truncated)?;
                *lens = rest;
                let (row, rest) = data
                    .split_at_checked(u16::from_be_bytes(*len) as usize)
                    .ok_or_else(|| invalid(rle_incomplete()))?;
                *data = rest;
                if let Some(out) = out {
                    unpack_bits(row, out).map_err(invalid)?;
                }
            }
            Plane::Fill(value) => {
                if let Some(out) = out {
                    out.fill(*value);
                }
            }
        }
        Ok(())
    }
}

/// 压缩方式是不认识的编号时的说明。
fn unknown_compression(code: u16) -> String {
    trf!("不认识的压缩方式（{code}）", "unknown compression method ({code})")
}

enum PixelsRead {
    Read,
    /// 像素是用 ZIP 压缩的，没读
    Zip,
}

/// 把一个图层的像素读进 `tiles`。图层的矩形不是空的。
fn read_layer_pixels(record: &Record, sample_bytes: usize, keep: Rect, tiles: &mut TileMap) -> io::Result<PixelsRead> {
    let rect = record.rect;
    let (w, h) = ((rect.x1 - rect.x0) as usize, (rect.y1 - rect.y0) as usize);
    let mut planes = Vec::with_capacity(4);
    for id in [0, 1, 2, -1] {
        planes.push(match record.channels.iter().find(|c| c.id == id) {
            Some(channel) => match Plane::open(channel.data, w * sample_bytes, h)? {
                Some(plane) => plane,
                None => return Ok(PixelsRead::Zip),
            },
            // 没有透明度通道的图层整个是不透明的。
            None if id == -1 => Plane::Fill(0xFF),
            None => return Err(invalid(tr("缺少颜色通道", "a color channel is missing"))),
        });
    }
    read_planes(&mut planes, rect, sample_bytes, keep, tiles)?;
    Ok(PixelsRead::Read)
}

/// 读一个图层的蒙版（规格 F-058）。蒙版的像素是用 ZIP 压缩的时候读不了，返回 None。
fn read_mask(record: &Record, info: &MaskInfo, sample_bytes: usize, keep: Rect) -> io::Result<Option<Mask>> {
    let default = if info.default >= 128 { ONE as u16 } else { 0 };
    let mut mask = Mask { tiles: GrayMap::default(), default, disabled: info.disabled };
    let r = info.rect;
    let Some(channel) = record.channels.iter().find(|c| c.id == info.channel) else { return Ok(Some(mask)) };
    if r.is_empty() {
        return Ok(Some(mask));
    }
    let (w, h) = ((r.x1 - r.x0) as usize, (r.y1 - r.y0) as usize);
    let row_bytes = w * sample_bytes;
    let Some(mut plane) = Plane::open(channel.data, row_bytes, h)? else { return Ok(None) };
    let mut row = vec![0u8; row_bytes];
    for y in r.y0..r.y1 {
        if y < keep.y0 || y >= keep.y1 {
            plane.next_row(row_bytes, None)?;
            continue;
        }
        plane.next_row(row_bytes, Some(&mut row))?;
        let ty = y.div_euclid(TILE);
        // 一次处理一个分块里的那一段，不必每个像素都去查表。
        let mut x = r.x0.max(keep.x0);
        let end = r.x1.min(keep.x1);
        while x < end {
            let tx = x.div_euclid(TILE);
            let stop = ((tx + 1) * TILE).min(end);
            let tile = mask.tiles.get_mut((tx, ty), default);
            for x in x..stop {
                let k = (x - r.x0) as usize;
                let v = if sample_bytes == 2 {
                    (u16::from_be_bytes([row[k * 2], row[k * 2 + 1]]) as u32 * ONE + 32767) / 65535
                } else {
                    (row[k] as u32 * ONE + 127) / 255
                };
                tile[((y - ty * TILE) * TILE + (x - tx * TILE)) as usize] = v as u16;
            }
            x = stop;
        }
    }
    Ok(Some(mask))
}

/// 把合并图像读成一个图层的像素。`r` 停在合并图像的开头。
fn read_merged(mut r: Bytes, channels: usize, sample_bytes: usize, canvas: Rect, notes: &mut Notes) -> io::Result<TileMap> {
    let (w, h) = (canvas.x1 as usize, canvas.y1 as usize);
    let row_bytes = w * sample_bytes;
    // 前三个通道是红、绿、蓝；有第四个的话当作透明度。
    let used = channels.min(4);
    let mut planes = Vec::with_capacity(4);
    match r.u16().map_err(|_| invalid(tr("文件里没有图像数据", "the file has no image data")))? {
        0 => {
            for _ in 0..used {
                planes.push(Plane::Raw(r.take(row_bytes * h)?));
            }
        }
        1 => {
            // 所有通道所有行的长度排在最前面，然后才是数据，一个通道接着一个通道。
            let table = r.take(channels * h * 2)?;
            let mut data = r.0;
            for lens in table.chunks_exact(h * 2).take(used) {
                planes.push(Plane::Rle { lens, data });
                let total: usize = lens.as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b) as usize).sum();
                data = data.get(total..).ok_or_else(truncated)?;
            }
        }
        2 | 3 => {
            return Err(invalid(tr("是用 ZIP 压缩的，本程序读不了", "it is ZIP-compressed, which this program cannot read")))
        }
        other => return Err(invalid(unknown_compression(other))),
    }
    if used == 4 {
        notes.add(tr("合并图像的第 4 个通道已当作透明度读入", "Read the 4th channel of the merged image as transparency"));
    } else {
        planes.push(Plane::Fill(0xFF));
    }
    let mut tiles = TileMap::default();
    read_planes(&mut planes, canvas, sample_bytes, canvas, &mut tiles)?;
    Ok(tiles)
}

/// 把矩形 `rect` 的像素读进 `tiles`，伸出 `area`（图层能放到的范围；合并图像是画布）的部分裁掉。
/// `planes` 依次是红、绿、蓝、透明度四个平面，每个采样 `sample_bytes` 个字节（1 或 2，大端）。
/// 四个平面一行一行地同时往下读，不用整幅的缓冲区。
fn read_planes(planes: &mut [Plane], rect: Rect, sample_bytes: usize, area: Rect, tiles: &mut TileMap) -> io::Result<()> {
    if rect.intersect(area).is_empty() {
        return Ok(());
    }
    let w = (rect.x1 - rect.x0) as usize;
    let row_bytes = w * sample_bytes;
    let mut rows = vec![vec![0u8; row_bytes]; planes.len()];
    let mut pixels = vec![[0u16; 4]; w];
    for y in rect.y0..rect.y1.min(area.y1) {
        if y < area.y0 {
            for plane in planes.iter_mut() {
                plane.next_row(row_bytes, None)?;
            }
            continue;
        }
        for (plane, row) in planes.iter_mut().zip(&mut rows) {
            plane.next_row(row_bytes, Some(row))?;
        }
        // 平面 c 是像素的第 c 个分量。8 位的值乘以 257 正好铺满 16 位。
        for (c, row) in rows.iter().enumerate().take(4) {
            if sample_bytes == 2 {
                for (px, v) in pixels.iter_mut().zip(row.as_chunks::<2>().0) {
                    px[c] = u16::from_be_bytes(*v);
                }
            } else {
                for (px, v) in pixels.iter_mut().zip(row) {
                    px[c] = *v as u16 * 257;
                }
            }
        }
        store_row(tiles, area, rect.x0 as i64, y, &pixels);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ora::shared::testing::*;
    use crate::ora::shared::to_premultiplied;
    use crate::ONE;

    /// 必须是 `InvalidData`，而且错误信息说的是预期的那件事。
    fn expect_invalid(result: io::Result<(Document, Vec<String>)>, keyword: &str) {
        let e = result.err().expect("应当报错");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        assert!(e.to_string().contains(keyword), "错误信息里应当有“{keyword}”：{e}");
    }

    fn expect_note(notes: &[String], keyword: &str) {
        assert!(notes.iter().any(|n| n.contains(keyword)), "说明里应当有“{keyword}”：{notes:?}");
    }

    // ---- 照着规范手工拼 PSD 文件 ----
    //
    // 下面这些函数不用上面写文件的代码，测的是"别的程序写的文件"。

    /// 要拼进文件的一个图层。
    #[derive(Clone)]
    struct Fix {
        /// 上、左、下、右
        rect: [i32; 4],
        /// 各通道的编号和数据（开头两个字节是压缩方式）
        channels: Vec<(i16, Vec<u8>)>,
        blend: [u8; 4],
        opacity: u8,
        clipping: u8,
        flags: u8,
        /// 蒙版数据（不含前面的长度）
        mask: Vec<u8>,
        /// Pascal 字符串形式的名字（不含长度字节）
        name: Vec<u8>,
        /// 附加信息块：键和数据
        blocks: Vec<([u8; 4], Vec<u8>)>,
    }

    impl Fix {
        /// 一个普通图层：`rect` 是上、左、下、右，四个通道都不压缩，像素全是 `rgba`。
        fn solid(name: &str, rect: [i32; 4], rgba: [u8; 4]) -> Fix {
            let n = ((rect[2] - rect[0]) * (rect[3] - rect[1])) as usize;
            let plane = |v: u8| raw(&vec![v; n]);
            Fix {
                rect,
                channels: vec![(-1, plane(rgba[3])), (0, plane(rgba[0])), (1, plane(rgba[1])), (2, plane(rgba[2]))],
                blend: *b"norm",
                opacity: 255,
                clipping: 0,
                flags: 8,
                mask: Vec::new(),
                name: name.as_bytes().to_vec(),
                blocks: Vec::new(),
            }
        }

        /// 没有像素的记录（图层组的两种记录、调整图层都是这样）。
        fn empty(name: &str) -> Fix {
            Fix::solid(name, [0; 4], [0; 4])
        }

        fn block(mut self, key: &[u8; 4], data: Vec<u8>) -> Fix {
            self.blocks.push((*key, data));
            self
        }

        fn with(mut self, change: impl FnOnce(&mut Fix)) -> Fix {
            change(&mut self);
            self
        }
    }

    /// 不压缩的通道数据。
    fn raw(plane: &[u8]) -> Vec<u8> {
        [&[0, 0], plane].concat()
    }

    /// RLE 压缩的通道数据：`plane` 每 `row` 个字节是一行。
    fn rle(plane: &[u8], row: usize) -> Vec<u8> {
        let (mut lens, mut data) = (Vec::new(), Vec::new());
        for line in plane.chunks(row) {
            let start = data.len();
            pack_bits(line, &mut data);
            lens.extend(((data.len() - start) as u16).to_be_bytes());
        }
        [&[0, 1], &lens[..], &data[..]].concat()
    }

    /// `luni` 块的数据。
    fn luni(name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut out = (units.len() as u32).to_be_bytes().to_vec();
        out.extend(units.iter().flat_map(|u| u.to_be_bytes()));
        out
    }

    /// `lsct` 块的数据：分隔记录的类型，后面是签名和混合模式。
    fn lsct(kind: u32) -> Vec<u8> {
        [&kind.to_be_bytes()[..], b"8BIM", b"pass"].concat()
    }

    /// "图层信息"：个数、记录、通道数据。`count_sign` 是 −1 时个数写成负的。
    fn layer_info(layers: &[Fix], count_sign: i16) -> Vec<u8> {
        let mut out = (layers.len() as i16 * count_sign).to_be_bytes().to_vec();
        for l in layers {
            out.extend(l.rect.iter().flat_map(|v| v.to_be_bytes()));
            out.extend((l.channels.len() as u16).to_be_bytes());
            for (id, data) in &l.channels {
                out.extend(id.to_be_bytes());
                out.extend((data.len() as u32).to_be_bytes());
            }
            out.extend(b"8BIM");
            out.extend(l.blend);
            out.extend([l.opacity, l.clipping, l.flags, 0]);
            let mut extra = (l.mask.len() as u32).to_be_bytes().to_vec();
            extra.extend(&l.mask);
            extra.extend([0; 4]); // 混合范围：长度 0
            extra.push(l.name.len() as u8);
            extra.extend(&l.name);
            extra.resize(extra.len().next_multiple_of(4), 0);
            for (key, data) in &l.blocks {
                extra.extend(b"8BIM");
                extra.extend(key);
                extra.extend((data.len() as u32).to_be_bytes());
                extra.extend(data);
                // 规范：补成偶数长度。
                extra.resize(extra.len().next_multiple_of(2), 0);
            }
            out.extend((extra.len() as u32).to_be_bytes());
            out.extend(extra);
        }
        for l in layers {
            for (_, data) in &l.channels {
                out.extend(data);
            }
        }
        out.resize(out.len().next_multiple_of(2), 0);
        out
    }

    /// 整个文件的几个部分，可以单独改动之后再拼起来。
    #[derive(Clone)]
    struct File {
        version: u16,
        channels: u16,
        height: u32,
        width: u32,
        depth: u16,
        mode: u16,
        resources: Vec<u8>,
        /// "图层与蒙版信息"一节的内容（不含前面的长度）
        layer_section: Vec<u8>,
        /// 合并图像，开头两个字节是压缩方式
        merged: Vec<u8>,
    }

    impl File {
        /// 8 位 RGB 的文件，合并图像是三个不压缩的平面，全是 `rgb`。
        fn new(width: u32, height: u32, layers: &[Fix], rgb: [u8; 3]) -> File {
            let n = (width * height) as usize;
            let info = layer_info(layers, 1);
            let layer_section =
                if layers.is_empty() { Vec::new() } else { [&(info.len() as u32).to_be_bytes()[..], &info, &[0; 4]].concat() };
            File {
                version: 1,
                channels: 3,
                height,
                width,
                depth: 8,
                mode: 3,
                resources: resolution_resource(96 << 16),
                layer_section,
                merged: raw(&[vec![rgb[0]; n], vec![rgb[1]; n], vec![rgb[2]; n]].concat()),
            }
        }

        fn with(mut self, change: impl FnOnce(&mut File)) -> File {
            change(&mut self);
            self
        }

        fn bytes(&self) -> Vec<u8> {
            let mut out = b"8BPS".to_vec();
            out.extend(self.version.to_be_bytes());
            out.extend([0; 6]);
            out.extend(self.channels.to_be_bytes());
            out.extend(self.height.to_be_bytes());
            out.extend(self.width.to_be_bytes());
            out.extend(self.depth.to_be_bytes());
            out.extend(self.mode.to_be_bytes());
            out.extend([0; 4]); // 颜色模式数据
            out.extend((self.resources.len() as u32).to_be_bytes());
            out.extend(&self.resources);
            out.extend((self.layer_section.len() as u32).to_be_bytes());
            out.extend(&self.layer_section);
            out.extend(&self.merged);
            out
        }

        fn parse(&self) -> io::Result<(Document, Vec<String>)> {
            parse(&self.bytes())
        }
    }

    /// 一个分辨率资源块，前面再放一个名字长度是奇数的别的资源，后面放一个数据长度是奇数的。
    fn resolution_resource(hres: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(b"8BIM\x04\x0B\x03abc\x00\x00\x00\x02hi"); // 1035 号资源，名字 "abc"（连长度 4 个字节，不用补）
        out.extend(b"8BIM\x03\xED\x00\x00\x00\x00\x00\x10");
        out.extend(hres.to_be_bytes());
        out.extend([0, 2, 0, 2]); // 显示单位选的是厘米，存的还是像素/英寸
        out.extend(hres.to_be_bytes());
        out.extend([0, 2, 0, 2]);
        out.extend(b"8BIM\x04\x0C\x00\x00\x00\x00\x00\x03xyz\x00"); // 数据 3 个字节，补一个
        out
    }

    // ---- 测试 ----

    #[test]
    fn round_trip_keeps_everything_up_to_eight_bits() {
        let doc = sample_document();
        let tmp = Temp::new("roundtrip.psd");
        save(&doc, &tmp.0).unwrap();
        let (back, notes) = load(&tmp.0).unwrap();
        assert_eq!(notes, Vec::<String>::new());
        // 不透明度在文件里只有 0 到 255 这 256 档。
        assert_same_after_round_trip(&doc, &back, 0.5 / 255.0 + 1e-6);
        assert_eq!(back.active(), doc.layers().len() - 1);
        // 伸出画布的内容跟过来了，离画布太远的没有。
        assert!(back.layers()[0].tiles.get((7, 7)).is_none());
        for (x, y) in [(-64, 0), (-1, 63), (255, 191)] {
            let (a, b) = (pixel(&doc.layers()[0], x, y), pixel(&back.layers()[0], x, y));
            assert!(a.iter().zip(b).all(|(p, q)| p.abs_diff(q) <= 130), "({x}, {y})：{a:?} 对 {b:?}");
        }
        assert!(back.layers()[1].tiles.is_empty());

        // 再存一次、再读一次，文档一点不变：第一次存盘已经把像素和不透明度都量化了。
        let again = Temp::new("again.psd");
        save(&back, &again.0).unwrap();
        let (third, _) = load(&again.0).unwrap();
        for (a, b) in back.layers().iter().zip(third.layers()) {
            assert_eq!(a.props, b.props);
            for y in 0..back.height {
                for x in 0..back.width {
                    assert_eq!(pixel(a, x, y), pixel(b, x, y), "图层“{}”的像素 ({x}, {y})", a.props.name);
                }
            }
        }
        assert_eq!(third.flatten_rgb(), back.flatten_rgb());
        // 两次写出来的文件一个字节都不差。
        assert!(std::fs::read(&tmp.0).unwrap() != std::fs::read(&again.0).unwrap(), "第一次存的是量化之前的合并图像");
        let once_more = Temp::new("once-more.psd");
        save(&third, &once_more.0).unwrap();
        assert!(std::fs::read(&again.0).unwrap() == std::fs::read(&once_more.0).unwrap());
    }

    /// 写出来的文件确实是规范要求的样子。这里不用上面读文件的代码，直接按偏移量看字节。
    #[test]
    fn written_file_follows_the_specification() {
        let mut layer = Layer::new("线稿 ✏️");
        layer.props = LayerProps { opacity: 0.5, visible: false, blend: BlendMode::Add, ..layer.props };
        for x in 10..13 {
            set_pixel(&mut layer, x, 20, [ONE as u16, 0, 0, ONE as u16]);
        }
        let mut doc = Document::from_layers(70, 30, [1, 2, 3], vec![layer, Layer::new("空")]);
        doc.dpi = 350;
        let tmp = Temp::new("layout.psd");
        save(&doc, &tmp.0).unwrap();
        let bytes = std::fs::read(&tmp.0).unwrap();
        let u32_at = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;

        // 文件头：签名、版本 1、保留、3 个通道、高 30、宽 70、8 位、RGB；颜色模式数据为空。
        assert_eq!(bytes[..30], *b"8BPS\x00\x01\0\0\0\0\0\0\x00\x03\x00\x00\x00\x1E\x00\x00\x00\x46\x00\x08\x00\x03\0\0\0\0");
        // 图像资源：只有 1005 号，350 像素/英寸。
        assert_eq!(bytes[30..62], *b"\x00\x00\x00\x1C8BIM\x03\xED\x00\x00\x00\x00\x00\x10\x01\x5E\x00\x00\x00\x01\x00\x01\x01\x5E\x00\x00\x00\x01\x00\x01");
        // 图层与蒙版信息：整节的长度、图层信息的长度（偶数）、3 个图层（背景加两个）。
        let (section_len, info_len) = (u32_at(62), u32_at(66));
        assert_eq!((section_len, info_len % 2), (info_len + 8, 0));
        assert_eq!(bytes[70..72], [0, 3]);
        // 整节之后是合并图像：RLE，3×30 行的长度，然后是数据，正好到文件尾。
        let merged = 66 + section_len;
        assert_eq!(bytes[merged - 4..merged + 2], [0, 0, 0, 0, 0, 1]);
        let table = &bytes[merged + 2..merged + 2 + 3 * 30 * 2];
        let total: usize = table.as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b) as usize).sum();
        assert_eq!(merged + 2 + table.len() + total, bytes.len());

        // 第一条记录是背景：盖满画布，四个通道，正常模式，不透明，可见。
        let record = &bytes[72..];
        assert_eq!(record[..18], *b"\0\0\0\0\0\0\0\0\x00\x00\x00\x1E\x00\x00\x00\x46\x00\x04");
        assert_eq!(
            (&record[18..20], &record[24..26], &record[30..32], &record[36..38]),
            (&[255, 255][..], &[0, 0][..], &[0, 1][..], &[0, 2][..])
        );
        assert_eq!(record[42..54], *b"8BIMnorm\xFF\x00\x08\x00");
        let extra = u32_at(72 + 54);
        // 蒙版长度 0；混合范围 40 个字节；名字"背景"（UTF-8 六个字节，连长度字节补到 8）；然后是 luni 块。
        assert_eq!(record[58..66], [0, 0, 0, 0, 0, 0, 0, 40]);
        assert_eq!(record[66..74], [0, 0, 255, 255, 0, 0, 255, 255]);
        assert_eq!(record[106..114], *b"\x06\xE8\x83\x8C\xE6\x99\xAF\x00");
        assert_eq!(record[114..134], *b"8BIMluni\x00\x00\x00\x08\x00\x00\x00\x02\x80\xCC\x66\x6F");
        assert_eq!(extra, 134 - 58);

        // 第二条记录：内容的包围盒是 (10, 20) 起 3×1；线性减淡、不透明度 128、隐藏。
        let record = &record[134..];
        assert_eq!(record[..18], *b"\x00\x00\x00\x14\x00\x00\x00\x0A\x00\x00\x00\x15\x00\x00\x00\x0D\x00\x04");
        assert_eq!(record[42..54], *b"8BIMlddg\x80\x00\x0A\x00");
        // 名字是 13 个字节的 UTF-8，连长度字节补到 16。铅笔是两个码位（U+270F U+FE0F），
        // 所以一共 5 个 UTF-16 码元，luni 块的数据补到 16 个字节。
        let name = "线稿 ✏️".as_bytes();
        assert_eq!((record[106] as usize, &record[107..120], &record[120..122]), (13, name, &[0, 0][..]));
        assert_eq!(
            record[122..150],
            *b"8BIMluni\x00\x00\x00\x10\x00\x00\x00\x05\x7E\xBF\x7A\x3F\x00\x20\x27\x0F\xFE\x0F\x00\x00"
        );
        // 每个通道一行：压缩方式 1、长度 2、数据"重复 3 次"。透明度和红色是 255，绿、蓝是 0。
        let lengths: Vec<usize> = (0..4).map(|k| u32_at(72 + 134 + 20 + k * 6)).collect();
        assert_eq!(lengths, [6, 6, 6, 6]);

        // 第三条记录是空图层：矩形全零，每个通道只有两个字节的"不压缩"。
        let record = &record[150..];
        assert_eq!(record[..18], *b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x00\x04");
        assert_eq!(record[42..54], *b"8BIMnorm\xFF\x00\x08\x00");
        let lengths: Vec<usize> = (0..4).map(|k| u32_at(72 + 134 + 150 + 20 + k * 6)).collect();
        assert_eq!(lengths, [2, 2, 2, 2]);
        // 三条记录之后是通道数据。背景的四个通道略过，看第二个图层的。
        let data = 72 + 134 + 150 + 54 + 4 + u32_at(72 + 134 + 150 + 54);
        let background: usize = (0..4).map(|k| u32_at(72 + 20 + k * 6)).sum();
        let pixels = &bytes[data + background..];
        assert_eq!(
            pixels[..24],
            *b"\x00\x01\x00\x02\xFE\xFF\x00\x01\x00\x02\xFE\xFF\x00\x01\x00\x02\xFE\x00\x00\x01\x00\x02\xFE\x00"
        );
        assert_eq!(pixels[24..32], [0; 8]);
        assert_eq!(data + background + 32, 66 + 4 + info_len);
    }

    #[test]
    fn pack_bits_round_trips_tricky_inputs() {
        let mut cases: Vec<Vec<u8>> =
            vec![vec![], vec![7], vec![7, 7], vec![7, 8], vec![7, 7, 7], vec![1, 2, 2, 3], vec![1, 2, 2, 2, 3]];
        // 各种长度的重复和不重复，重点是 128 这道坎的两边。
        for n in [1, 2, 3, 126, 127, 128, 129, 130, 255, 256, 257, 300, 1000] {
            cases.push(vec![9; n]);
            cases.push((0..n).map(|i| (i % 251) as u8).collect());
            cases.push((0..n).map(|i| (i / 2 % 251) as u8).collect()); // 全是两个一组的重复
            cases.push([vec![1, 2, 3], vec![9; n], vec![4, 5]].concat());
            cases.push([vec![9; n], vec![1], vec![9; n]].concat());
            cases.push([(0..n).map(|i| (i % 7) as u8).collect::<Vec<u8>>(), vec![5; n], vec![6; 2], vec![1, 2, 3]].concat());
        }
        let mut rng = Rng(0xBADC_0FFE);
        for round in 0..300 {
            // 随机的数据，取值范围很小，所以长短不一的重复很多。
            let len = rng.next() as usize % 700;
            let spread = 1 + round % 5;
            cases.push((0..len).map(|_| (rng.next() % spread) as u8).collect());
        }
        for case in &cases {
            let mut packed = Vec::new();
            pack_bits(case, &mut packed);
            assert!(packed.len() <= case.len() + case.len().div_ceil(128), "{} 个字节压缩成了 {} 个", case.len(), packed.len());
            let mut back = vec![0xEE; case.len()];
            unpack_bits(&packed, &mut back).unwrap_or_else(|e| panic!("{e}：{case:?}"));
            assert_eq!(&back, case);
            // 写出来的数据里没有"什么也不做"的 −128，每一段的长度也都对得上：按段走一遍正好走完。
            let mut at = 0;
            while at < packed.len() {
                assert_ne!(packed[at], 0x80);
                at += if packed[at] < 128 { packed[at] as usize + 2 } else { 2 };
            }
            assert_eq!(at, packed.len());
        }
        // 具体的字节。
        let packed = |row: &[u8]| {
            let mut out = Vec::new();
            pack_bits(row, &mut out);
            out
        };
        assert_eq!(packed(&[5; 128]), [0x81, 5]);
        assert_eq!(packed(&[5; 129]), [0x81, 5, 0, 5]);
        assert_eq!(packed(&[5; 130]), [0x81, 5, 0xFF, 5]);
        assert_eq!(packed(&[1, 2, 3]), [2, 1, 2, 3]);
        assert_eq!(packed(&[1, 1, 2, 3]), [0xFF, 1, 1, 2, 3]);
        assert_eq!(packed(&[1, 2, 2, 3]), [3, 1, 2, 2, 3]);
        assert_eq!(packed(&[1, 2, 2, 2, 3]), [0, 1, 0xFE, 2, 0, 3]);
        assert_eq!(packed(&(0..129).collect::<Vec<u8>>())[..2], [127, 0]);
        assert_eq!(packed(&(0..129).collect::<Vec<u8>>())[129..], [0, 128]);

        // Apple 的技术说明 TN1023 里的例子，里面还插了一个 −128。
        let example = [0xFE, 0xAA, 0x02, 0x80, 0x00, 0x2A, 0xFD, 0xAA, 0x03, 0x80, 0x00, 0x2A, 0x22, 0x80, 0xF7, 0xAA];
        let expect = [
            0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0x22, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        let mut out = [0u8; 24];
        unpack_bits(&example, &mut out).unwrap();
        assert_eq!(out, expect);
        // 坏数据：不够、太长。多出来的输入不算错。
        assert!(unpack_bits(&example[..15], &mut out).is_err());
        assert!(unpack_bits(&example[..11], &mut out).is_err());
        assert!(unpack_bits(&[], &mut out).is_err());
        assert!(unpack_bits(&[0x80, 0x80], &mut out).is_err());
        assert!(unpack_bits(&example, &mut [0u8; 23]).is_err());
        assert!(unpack_bits(&[3, 1, 2, 3, 4], &mut [0u8; 3]).is_err());
        assert!(unpack_bits(&example, &mut [0u8; 14]).is_ok());
        assert!(unpack_bits(&[], &mut []).is_ok());
    }

    /// 8 位的图像读进来、写出去、再读进来，两次读到的东西完全一样；写出去的像素也没变。
    #[test]
    fn an_eight_bit_image_is_stable_through_import_and_export() {
        let (w, h) = (150usize, 70usize);
        let mut rng = Rng(0x1234_5678);
        let source: Vec<[u8; 4]> = (0..w * h).map(|_| rng.pixel8()).collect();
        let plane = |c: usize| source.iter().map(|px| px[c]).collect::<Vec<u8>>();
        let mut layer = Fix::solid("导入", [0, 0, h as i32, w as i32], [0; 4]);
        // 两个通道不压缩，两个用 RLE。
        layer.channels = vec![(-1, rle(&plane(3), w)), (0, raw(&plane(0))), (1, rle(&plane(1), w)), (2, raw(&plane(2)))];
        let (first, notes) = File::new(w as u32, h as u32, &[layer], [0; 3]).parse().unwrap();
        assert_eq!(notes, Vec::<String>::new());

        let tmp = Temp::new("stable.psd");
        save(&first, &tmp.0).unwrap();
        let (second, _) = load(&tmp.0).unwrap();
        assert_eq!((second.layers().len(), second.background), (1, [255; 3]));
        let (a, b) = (&first.layers()[0], &second.layers()[0]);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                assert_eq!(pixel(a, x, y), pixel(b, x, y), "({x}, {y})");
            }
        }
        // 文件里的像素：透明度不小于 2/255 的原样不变；1/255 的颜色最多差 1；全透明的颜色归零。
        let exported = straight8(a, first.bounds());
        for (src, out) in source.iter().zip(exported.as_chunks::<4>().0) {
            match src[3] {
                0 => assert_eq!(*out, [0; 4]),
                1 => assert!(out[3] == 1 && (0..3).all(|i| out[i].abs_diff(src[i]) <= 1), "{src:?} -> {out:?}"),
                _ => assert_eq!(out, src),
            }
        }
    }

    /// 一个别的程序可能写出来的文件：图层组、Unicode 名字、蒙版、文字图层、调整图层、出界的图层……
    #[test]
    fn loads_a_file_written_by_another_tool() {
        let layers = [
            // 最底下：没有透明度通道的图层，盖满画布，RLE 压缩。Pascal 名字是乱码，以 luni 为准。
            Fix::solid("\u{FFFD}", [0, 0, 40, 100], [0; 4])
                .with(|l| {
                    l.channels = vec![(0, rle(&[200; 4000], 100)), (1, rle(&[100; 4000], 100)), (2, rle(&[50; 4000], 100))];
                    l.name = vec![0xB5, 0xD7];
                })
                .block(b"luni", luni("底色")),
            // 一个图层组：起点的隐藏记录、两个图层、组本身（穿透、隐藏，不透明度 128）。
            Fix::empty("</Layer group>").block(b"lsct", lsct(3)),
            // 左上角伸出画布的图层，带蒙版（−2 号通道和蒙版数据：左上角 3×2 是白的，其余默认是黑的）和图层样式；
            // 名字后面跟着一个不认识的块。
            Fix::solid("masked", [-5, -10, 5, 10], [10, 20, 30, 255])
                .with(|l| {
                    l.channels.push((-2, raw(&[255; 6])));
                    l.mask = [&[0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 3][..], &[0, 0, 0, 0]].concat();
                    l.opacity = 128;
                })
                .block(b"zzzz", vec![1, 2, 3])
                .block(b"luni", luni("带蒙版 𝄞"))
                .block(b"lfx2", vec![0; 8]),
            // 文字图层：像素留着；混合模式是本程序没有的"颜色加深"；剪贴到下面的图层。
            Fix::solid("Text", [10, 90, 12, 110], [0, 0, 255, 128])
                .with(|l| {
                    l.blend = *b"idiv";
                    l.clipping = 1;
                    l.flags = 8 | 2;
                })
                .block(b"TySh", vec![0; 20]),
            Fix::empty("组一").block(b"lsct", lsct(1)).with(|l| {
                l.flags = 8 | 2;
                l.opacity = 128;
                l.blend = *b"pass";
            }),
            // 调整图层跳过；纯色填充的描述符读不懂（版本不对）、还带着矢量蒙版：留着，显示成透明。
            Fix::empty("Curves 1").block(b"curv", vec![0; 4]),
            Fix::empty("Color Fill 1").block(b"SoCo", vec![0; 4]).block(b"vmsk", vec![0; 8]),
            // 存着像素、但标明了"像素与外观无关"的两个图层：填充图层的像素照读（那就是画好的样子），另一个读成空图层。
            Fix::solid("Gradient Fill 1", [0, 0, 40, 100], [255; 4]).with(|l| l.flags = 8 | 16).block(b"GdFl", vec![0; 4]),
            Fix::solid("frame", [0, 0, 40, 100], [255; 4]).with(|l| l.flags = 8 | 16),
            // 最上面：滤色模式的智能对象，luni 的字符数把结尾的 0 也算了进去。
            Fix::solid("top", [38, 0, 40, 2], [255, 255, 255, 255])
                .with(|l| l.blend = *b"scrn")
                .block(b"luni", [luni("顶层"), vec![0, 0]].concat().with_count(3))
                .block(b"SoLd", vec![0; 4]),
        ];
        let (doc, notes) = File::new(100, 40, &layers, [9; 3]).parse().unwrap();

        assert_eq!((doc.width, doc.height, doc.dpi, doc.background), (100, 40, 96, [255; 3]));
        let props: Vec<_> = doc
            .layers()
            .iter()
            .map(|l| (l.props.name.as_str(), l.depth, l.props.opacity, l.props.visible, l.props.blend, l.props.pass_through))
            .collect();
        let half = 128.0 / 255.0;
        assert_eq!(
            props,
            [
                ("底色", 0, 1.0, true, BlendMode::Normal, false),
                ("带蒙版 𝄞", 1, half, true, BlendMode::Normal, false),
                ("Text", 1, 1.0, false, BlendMode::Normal, false),
                // 组读成文件夹：隐藏、不透明度 128/255、穿透。
                ("组一", 0, half, false, BlendMode::Normal, true),
                ("Color Fill 1", 0, 1.0, true, BlendMode::Normal, false),
                ("Gradient Fill 1", 0, 1.0, true, BlendMode::Normal, false),
                ("frame", 0, 1.0, true, BlendMode::Normal, false),
                ("顶层", 0, 1.0, true, BlendMode::Screen, false),
            ]
        );
        let layer = |i: usize| &doc.layers()[i];
        assert_eq!(layer(3).kind, Kind::Folder { open: true });
        for i in [4, 5] {
            let Kind::Fill(fill) = &layer(i).kind else { panic!("第 {i} 层应当是填充图层") };
            assert_eq!(fill.paint, Paint::Other);
            assert!(fill.psd.is_some());
        }
        assert!(layer(4).tiles.is_empty());
        assert_eq!(content_bounds(&layer(5).tiles, doc.bounds()), doc.bounds());
        let import = |px: [u8; 4]| to_premultiplied(px.map(|v| v as u16 * 257));
        assert_eq!((pixel(layer(0), 0, 0), pixel(layer(0), 99, 39)), (import([200, 100, 50, 255]), import([200, 100, 50, 255])));
        assert_eq!(content_bounds(&layer(1).tiles, doc.bounds()), Rect { x0: 0, y0: 0, x1: 10, y1: 5 });
        assert_eq!(pixel(layer(1), 9, 4), import([10, 20, 30, 255]));
        // 蒙版：左上角 3×2 是白的，别处是默认的黑。
        let mask = layer(1).mask.as_ref().expect("应当读到蒙版");
        assert_eq!((mask.default, mask.disabled), (0, false));
        assert_eq!(mask.tile((0, 0)).map(|t| (t[0], t[2], t[64 + 2], t[3], t[2 * 64])), Some((ONE as u16, ONE as u16, ONE as u16, 0, 0)));
        assert_eq!(content_bounds(&layer(2).tiles, doc.bounds()), Rect { x0: 90, y0: 10, x1: 100, y1: 12 });
        assert_eq!(pixel(layer(2), 99, 11), import([0, 0, 255, 128]));
        assert!(layer(6).tiles.is_empty());
        assert_eq!(content_bounds(&layer(7).tiles, doc.bounds()), Rect { x0: 0, y0: 38, x1: 2, y1: 40 });

        // 剪贴在下面图层上的那一层，剪贴的开关照原样读进来。
        assert!(layer(2).props.clip && !layer(1).props.clip);

        assert_eq!(notes.len(), 9, "{notes:?}");
        expect_note(&notes, "矢量蒙版被忽略了：“Color Fill 1”");
        // 样式的块是乱写的：读不懂，原样留着。
        expect_note(&notes, "图层样式读不懂，不显示；存回 PSD 时原样保留：“带蒙版 𝄞”");
        assert!(layer(1).style.as_ref().is_some_and(|s| s.psd.iter().any(|(k, _)| k == b"lfx2") && s.effects.is_empty()));
        expect_note(&notes, "文字图层只读入了像素，文字不能再编辑：“Text”");
        expect_note(&notes, "已改为“正常”：“Text（idiv）”");
        expect_note(&notes, "调整图层本程序没有对应的功能，已跳过：“Curves 1”");
        expect_note(&notes, "显示成透明；存回 PSD 时原样保留：“Color Fill 1”");
        expect_note(&notes, "显示的是文件里存着的像素；存回 PSD 时原样保留：“Gradient Fill 1”");
        expect_note(&notes, "标明了与画面无关，没有读入：“frame”");
        expect_note(&notes, "智能对象只读入了像素：“顶层”");
    }

    /// 测试里用：把 `luni` 数据开头的字符数改掉。
    trait WithCount {
        fn with_count(self, count: u32) -> Self;
    }

    impl WithCount for Vec<u8> {
        fn with_count(mut self, count: u32) -> Self {
            self[..4].copy_from_slice(&count.to_be_bytes());
            self
        }
    }

    #[test]
    fn files_without_layers_load_the_merged_image() {
        // 没有图层与蒙版信息：合并图像成了唯一的图层。
        let file = File::new(70, 3, &[], [10, 20, 30]);
        let (doc, notes) = file.parse().unwrap();
        assert_eq!((doc.layers().len(), doc.layers()[0].props.name.as_str(), notes.len()), (1, "图层 1", 0));
        let opaque = to_premultiplied([10 * 257, 20 * 257, 30 * 257, 65535]);
        assert_eq!((pixel(&doc.layers()[0], 0, 0), pixel(&doc.layers()[0], 69, 2)), (opaque, opaque));
        // 这一节有、但里面是空的，也一样。
        for section in [vec![0; 4], vec![0; 8], vec![0, 0, 0, 2, 0, 0, 0, 0, 0, 0]] {
            let (doc, notes) = file.clone().with(|f| f.layer_section = section).parse().unwrap();
            assert_eq!((pixel(&doc.layers()[0], 69, 2), notes.len()), (opaque, 0));
        }

        // RLE 压缩的合并图像，四个通道：第四个当透明度，并且交代一句。
        let planes: [Vec<u8>; 4] = [
            vec![255; 210],
            vec![0; 210],
            (0..210).map(|i| i as u8).collect(),
            [vec![128; 70], vec![0; 70], vec![255; 70]].concat(),
        ];
        let mut merged = vec![0, 1];
        let packed: Vec<Vec<u8>> = planes.iter().map(|p| rle(p, 70)).collect();
        for p in &packed {
            merged.extend(&p[2..2 + 6]);
        }
        for p in &packed {
            merged.extend(&p[2 + 6..]);
        }
        let rgba = file.clone().with(|f| (f.channels, f.merged) = (4, merged.clone()));
        let (doc, notes) = rgba.parse().unwrap();
        expect_note(&notes, "第 4 个通道已当作透明度");
        let layer = &doc.layers()[0];
        assert_eq!(pixel(layer, 5, 0), to_premultiplied([65535, 0, 5 * 257, 128 * 257]));
        assert_eq!(pixel(layer, 5, 1), [0; 4]);
        assert_eq!(pixel(layer, 69, 2), to_premultiplied([65535, 0, 209 * 257, 65535]));
        // 多出来的通道（第五个起）不管。
        let five = rgba.clone().with(|f| {
            f.channels = 5;
            f.merged = [&[0, 1][..], &merged[2..26], &[0, 2, 0, 2, 0, 2], &merged[26..], &[0xBB, 7].repeat(3)].concat();
        });
        assert_eq!(pixel(&five.parse().unwrap().0.layers()[0], 69, 2), to_premultiplied([65535, 0, 209 * 257, 65535]));

        // 合并图像不完整、压缩方式不对。
        expect_invalid(file.clone().with(|f| f.merged.truncate(100)).parse(), "合并图像");
        expect_invalid(file.clone().with(|f| f.merged.clear()).parse(), "没有图像数据");
        expect_invalid(rgba.clone().with(|f| f.merged.truncate(20)).parse(), "合并图像");
        expect_invalid(rgba.clone().with(|f| f.merged.truncate(merged.len() - 1)).parse(), "合并图像");
        expect_invalid(file.clone().with(|f| f.merged[1] = 2).parse(), "ZIP");
        expect_invalid(file.clone().with(|f| f.merged[1] = 9).parse(), "不认识的压缩方式（9）");

        // 有图层、但一个能读的都没有（这里是只有一个调整图层）：也用合并图像，并且交代一句。
        let only = [Fix::empty("Levels 1").block(b"levl", vec![0; 4])];
        let (doc, notes) = File::new(70, 3, &only, [10, 20, 30]).parse().unwrap();
        assert_eq!((doc.layers().len(), pixel(&doc.layers()[0], 69, 2)), (1, opaque));
        expect_note(&notes, "没有本程序能读的图层，读入的是合并之后的图像");
        // 只有一个空图层的文件，读进来就是一个空图层，不去碰合并图像。
        let (doc, notes) = File::new(70, 3, &[Fix::empty("空的")], [10, 20, 30]).with(|f| f.merged.clear()).parse().unwrap();
        assert_eq!((doc.layers()[0].props.name.as_str(), doc.layers()[0].tiles.is_empty(), notes.len()), ("空的", true, 0));
    }

    #[test]
    fn sixteen_bit_files_are_converted() {
        // 每通道 16 位的文件：图层信息在 Lr16 块里，前面那个"图层信息"是空的。个数写成负的。
        let n = 6 * 4;
        let plane = |v: u16| v.to_be_bytes().repeat(n);
        let mut layer = Fix::solid("十六位", [1, 2, 5, 8], [0; 4]);
        layer.channels =
            vec![(-1, raw(&plane(0x8000))), (0, rle(&plane(0xFFFF), 12)), (1, raw(&plane(0x1234))), (2, rle(&plane(0), 12))];
        let info = layer_info(&[layer], -1);
        let block = [&b"8BIMLr16"[..], &(info.len() as u32).to_be_bytes(), &info].concat();
        let merged = raw(&0x4000u16.to_be_bytes().repeat(3 * 70 * 9));
        let file = File::new(70, 9, &[], [0; 3]).with(|f| {
            f.depth = 16;
            f.layer_section = [&[0u8; 8][..], &block].concat();
            f.merged = merged.clone();
        });
        let (doc, notes) = file.parse().unwrap();
        expect_note(&notes, "每通道 16 位");
        assert_eq!((doc.layers().len(), doc.layers()[0].props.name.as_str(), notes.len()), (1, "十六位", 1));
        let layer = &doc.layers()[0];
        assert_eq!(content_bounds(&layer.tiles, doc.bounds()), Rect { x0: 2, y0: 1, x1: 8, y1: 5 });
        // 直接从 16 位换算，比先砍成 8 位精确。
        assert_eq!(pixel(layer, 2, 1), to_premultiplied([0xFFFF, 0x1234, 0, 0x8000]));
        assert_eq!(pixel(layer, 7, 4), [16384, 1165, 0, 16384]);

        // 没有图层的 16 位文件：合并图像。
        let (doc, _) = file.clone().with(|f| f.layer_section.clear()).parse().unwrap();
        assert_eq!(pixel(&doc.layers()[0], 69, 8), to_premultiplied([0x4000, 0x4000, 0x4000, 0xFFFF]));
        // Lr16 块的长度写得不对（比实际的长）也能读。
        let long = file.clone().with(|f| f.layer_section[16..20].copy_from_slice(&[0, 9, 0, 0]));
        assert_eq!(long.parse().unwrap().0.layers()[0].props.name, "十六位");
        // 图层的像素是 ZIP 压缩的（Photoshop 存 16 位文件时就是这样）：读不了，退回到合并图像。
        let mut zipped = Fix::solid("压缩的", [1, 2, 5, 8], [0; 4]);
        zipped.channels.iter_mut().for_each(|(_, data)| *data = vec![0, 3, 0x78, 0x9C, 1, 2, 3]);
        let info = layer_info(&[zipped], 1);
        let block = [&b"8BIMLr16"[..], &(info.len() as u32).to_be_bytes(), &info].concat();
        let (doc, notes) = file.clone().with(|f| f.layer_section = [&[0u8; 8][..], &block].concat()).parse().unwrap();
        assert_eq!((doc.layers().len(), doc.layers()[0].props.name.as_str()), (1, "图层 1"));
        expect_note(&notes, "ZIP 压缩的，本程序读不了，已跳过：“压缩的”");
        expect_note(&notes, "读入的是合并之后的图像");
    }

    #[test]
    fn background_layer_becomes_the_background_colour_only_when_it_really_is_one() {
        let top = Fix::solid("画", [1, 1, 3, 3], [255, 0, 0, 255]);
        let load_with = |bottom: Fix| File::new(70, 66, &[bottom, top.clone()], [0; 3]).parse().unwrap().0;
        let solid = Fix::solid("背景", [0, 0, 66, 70], [30, 60, 90, 255]);

        let doc = load_with(solid.clone());
        assert_eq!((doc.background, doc.layers().len(), doc.layers()[0].props.name.as_str()), ([30, 60, 90], 1, "画"));
        // 图层比画布大也行；没有透明度通道也行。
        assert_eq!(load_with(Fix::solid("背景", [-2, -2, 70, 80], [1, 2, 3, 255])).background, [1, 2, 3]);
        assert_eq!(load_with(solid.clone().with(|l| drop(l.channels.remove(0)))).background, [30, 60, 90]);
        // 英文界面下存的文件里，这个图层叫 "Background"，不管现在是哪种语言都认。
        assert_eq!(load_with(solid.clone().with(|l| l.name = b"Background".to_vec())).background, [30, 60, 90]);
        assert_eq!(in_english(|| load_with(solid.clone())).background, [30, 60, 90]);
        // 名字以 luni 为准。
        assert_eq!(
            load_with(solid.clone().with(|l| l.name = b"Paper".to_vec()).block(b"luni", luni("背景"))).background,
            [30, 60, 90]
        );

        // 下面这些都不算：照旧是一个普通图层，背景色是白的。
        let kept = |doc: Document| (doc.background, doc.layers().len()) == ([255; 3], 2);
        assert!(kept(load_with(solid.clone().with(|l| l.name = b"Paper".to_vec()))), "名字不对");
        assert!(kept(load_with(solid.clone().with(|l| l.name = b"background".to_vec()))), "名字不对");
        assert!(kept(load_with(solid.clone().block(b"luni", luni("Paper")))), "名字以 luni 为准");
        assert!(kept(load_with(solid.clone().with(|l| l.flags |= 2))), "隐藏的");
        assert!(kept(load_with(solid.clone().with(|l| l.opacity = 254))), "半透明的");
        assert!(kept(load_with(solid.clone().with(|l| l.blend = *b"mul "))), "不是正常模式");
        assert!(kept(load_with(Fix::solid("背景", [0, 0, 66, 69], [30, 60, 90, 255]))), "没盖满画布");
        assert!(kept(load_with(Fix::solid("背景", [1, 0, 66, 70], [30, 60, 90, 255]))), "没盖满画布");
        assert!(kept(load_with(Fix::solid("背景", [0, 0, 66, 70], [30, 60, 90, 254]))), "不是完全不透明");
        let last = 66 * 70 + 1;
        assert!(kept(load_with(solid.clone().with(|l| l.channels[2].1[last] = 61))), "不止一种颜色");
    }

    /// 背景图层的名字跟着界面语言走；换了语言之后，以前存的文件照样还原出背景色。
    #[test]
    fn background_colour_survives_a_change_of_language() {
        let doc = sample_document();
        let step = 0.5 / 255.0 + 1e-6;
        // 第一条图层记录是背景，名字在记录里的位置见 `written_file_follows_the_specification`。
        let has_background_named = |path: &Path, name: &str| {
            let bytes = std::fs::read(path).unwrap();
            let pascal = [&[name.len() as u8][..], name.as_bytes()].concat();
            bytes[72 + 106..].starts_with(&pascal)
        };

        // 英文界面下存，英文、中文界面下读。
        let english = Temp::new("english.psd");
        let (back, notes) = in_english(|| {
            save(&doc, &english.0).unwrap();
            load(&english.0).unwrap()
        });
        assert!(has_background_named(&english.0, "Background"));
        assert_eq!(notes, Vec::<String>::new());
        assert_same_after_round_trip(&doc, &back, step);
        assert_same_after_round_trip(&doc, &load(&english.0).unwrap().0, step);

        // 中文界面下存，英文界面下读。
        let chinese = Temp::new("chinese.psd");
        save(&doc, &chinese.0).unwrap();
        assert!(has_background_named(&chinese.0, "背景"));
        let (back, notes) = in_english(|| load(&chinese.0)).unwrap();
        assert_eq!(notes, Vec::<String>::new());
        assert_same_after_round_trip(&doc, &back, step);
    }

    #[test]
    fn notes_and_errors_in_english() {
        let layers = [
            Fix::solid("", [0, 0, 2, 2], [1, 2, 3, 255]),
            Fix::solid("a", [-1, 0, 2, 2], [1, 2, 3, 255]).with(|l| l.channels.push((-2, raw(&[255; 6])))),
            Fix::solid("b", [0, 0, 2, 9], [1, 2, 3, 255]).with(|l| l.blend = *b"idiv"),
            Fix::empty("Curves 1").block(b"curv", vec![0; 4]),
            Fix::empty("组").block(b"lsct", lsct(1)),
        ];
        let (doc, notes) = in_english(|| File::new(4, 4, &layers, [0; 3]).parse()).unwrap();
        let names: Vec<&str> = doc.layers().iter().map(|l| l.props.name.as_str()).collect();
        // 没有起点的组记录读成一个空文件夹。"a" 有 −2 号通道却没有蒙版数据，不算蒙版。
        assert_eq!(names, ["Layer 1", "a", "b", "组"]);
        assert!(doc.layers()[1].mask.is_none());
        assert_eq!(
            notes,
            [
                r#"Cropped the parts of these layers that reach more than a canvas width or height beyond the canvas: "b""#,
                r#"Layers whose blend mode this program does not have were set to Normal: "b (idiv)""#,
                r#"Skipped adjustment layers, which this program does not support: "Curves 1""#,
            ]
        );
        // 没有能读的图层，读的是合并图像：图层的名字也是英文的。
        let only = [Fix::empty("Levels 1").block(b"levl", vec![0; 4])];
        let (doc, notes) = in_english(|| File::new(4, 4, &only, [0; 3]).parse()).unwrap();
        assert_eq!(doc.layers()[0].props.name, "Layer 1");
        assert_eq!(
            notes,
            [
                "The file has no layers this program can read; loaded the merged image instead",
                r#"Skipped adjustment layers, which this program does not support: "Levels 1""#,
            ]
        );

        // 坏文件的错误信息。
        let file = File::new(10, 8, &[Fix::solid("图层", [0, 0, 4, 6], [1, 2, 3, 255])], [0; 3]);
        let error = |file: File| in_english(|| file.parse()).err().unwrap().to_string();
        assert_eq!(
            error(file.clone().with(|f| f.mode = 1)),
            "The color mode of this PSD is Grayscale; this program can only read RGB files"
        );
        assert_eq!(
            error(file.clone().with(|f| f.mode = 5)),
            "The color mode of this PSD is unknown (5); this program can only read RGB files"
        );
        assert_eq!(
            error(file.clone().with(|f| f.version = 2)),
            "This is a PSB (Large Document Format) file; this program can only read regular PSD files"
        );
        assert_eq!(error(file.clone().with(|f| f.layer_section.truncate(30))), "The PSD file is incomplete or damaged");
        let mut rle_cut = Fix::solid("图层", [0, 0, 4, 6], [1, 2, 3, 255]);
        rle_cut.channels[2].1 = rle(&[5; 24], 6);
        rle_cut.channels[2].1.pop();
        assert_eq!(error(File::new(10, 8, &[rle_cut], [0; 3])), r#"Layer "图层": RLE data is incomplete"#);
        assert_eq!(
            error(file.clone().with(|f| f.merged[1] = 9).with(|f| f.layer_section.clear())),
            "Merged image: unknown compression method (9)"
        );
        // 换回中文之后还是原来的中文。
        expect_invalid(file.clone().with(|f| f.mode = 1).parse(), "这个 PSD 的颜色模式是灰度，本程序只能读 RGB 模式的文件");
    }

    #[test]
    fn rejects_files_that_are_not_readable_psd() {
        let layers = [Fix::solid("a", [0, 0, 4, 6], [1, 2, 3, 255]), Fix::solid("b", [2, 2, 4, 4], [9, 9, 9, 128])];
        let file = File::new(10, 8, &layers, [0; 3]);
        let good = file.bytes();
        assert_eq!(file.parse().unwrap().0.layers().len(), 2);

        // 不是 PSD。
        expect_invalid(parse(b""), "不是 PSD 文件");
        expect_invalid(parse(b"8BP"), "不是 PSD 文件");
        expect_invalid(parse(b"\x89PNG\r\n\x1a\n and so on, long enough to hold a header"), "不是 PSD 文件");
        expect_invalid(parse(&[b"8BIM", &good[4..]].concat()), "不是 PSD 文件");
        let tmp = Temp::new("missing.psd");
        assert_eq!(load(&tmp.0).err().expect("文件不存在").kind(), io::ErrorKind::NotFound);
        // 截断在任何位置都报错，不崩溃。（合并图像用不上，截在它里面不算错。）
        let merged_at = good.len() - (2 + 3 * 80);
        for cut in 0..good.len() {
            let result = parse(&good[..cut]);
            assert_eq!(result.is_ok(), cut >= merged_at, "截到 {cut} 个字节");
            if let Err(e) = result {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "截到 {cut} 个字节：{e}");
            }
        }

        // 版本。
        expect_invalid(file.clone().with(|f| f.version = 2).parse(), "PSB");
        expect_invalid(file.clone().with(|f| f.version = 0).parse(), "版本号是 0");
        expect_invalid(file.clone().with(|f| f.version = 3).parse(), "版本号是 3");
        // 尺寸。
        expect_invalid(file.clone().with(|f| f.width = 0).parse(), "画布尺寸 0×8");
        expect_invalid(file.clone().with(|f| f.height = 0).parse(), "画布尺寸");
        expect_invalid(file.clone().with(|f| f.width = MAX_SIDE as u32 + 1).parse(), "画布尺寸 16385×8");
        expect_invalid(file.clone().with(|f| f.height = 30000).parse(), "画布尺寸");
        expect_invalid(file.clone().with(|f| f.height = u32::MAX).parse(), "画布尺寸");
        // 颜色模式和位深。
        for (mode, name) in
            [(0, "位图"), (1, "灰度"), (2, "索引颜色"), (4, "CMYK"), (7, "多通道"), (8, "双色调"), (9, "Lab"), (5, "未知（5）")]
        {
            expect_invalid(file.clone().with(|f| f.mode = mode).parse(), &format!("颜色模式是{name}，"));
        }
        expect_invalid(file.clone().with(|f| (f.mode, f.depth) = (0, 1)).parse(), "位图");
        expect_invalid(file.clone().with(|f| f.depth = 32).parse(), "每通道 32 位");
        expect_invalid(file.clone().with(|f| f.depth = 1).parse(), "每通道 1 位");
        expect_invalid(file.clone().with(|f| f.channels = 2).parse(), "通道数不对（2）");
        expect_invalid(file.clone().with(|f| f.channels = 57).parse(), "通道数不对（57）");
        // 各节的长度超出了文件。
        expect_invalid(parse(&[&good[..26], &[0xFF; 4][..], &good[30..]].concat()), "不完整");
        expect_invalid(
            file.clone().with(|f| f.resources = vec![0; 3]).with(|f| f.merged.clear()).with(|f| f.layer_section.clear()).parse(),
            "没有图像数据",
        );
        let section_at = 26 + 4 + 4 + file.resources.len();
        expect_invalid(parse(&[&good[..section_at], &[0x7F, 0xFF, 0xFF, 0xFF][..], &good[section_at + 4..]].concat()), "不完整");
        expect_invalid(file.clone().with(|f| f.layer_section[..4].copy_from_slice(&[0, 1, 0, 0])).parse(), "不完整");

        // 分辨率资源坏了不算错，用默认值。
        assert_eq!(file.parse().unwrap().0.dpi, 96);
        for resources in [
            Vec::new(),
            vec![1, 2, 3],
            b"8BIM\x03\xED\x00\x00\x00\x00\x00\x10\x00\x48".to_vec(),
            b"8BIM\x03\xED\x00\x00\x00\x00\x00\x02\x00\x00".to_vec(),
        ] {
            assert_eq!(file.clone().with(|f| f.resources = resources).parse().unwrap().0.dpi, 72);
        }
        // 四舍五入到整数；16.16 定点数的上限是 65535。
        assert_eq!(file.clone().with(|f| f.resources = resolution_resource(0x012C_8000)).parse().unwrap().0.dpi, 301);
        assert_eq!(file.clone().with(|f| f.resources = resolution_resource(u32::MAX)).parse().unwrap().0.dpi, 65536);
    }

    #[test]
    fn rejects_bad_layer_records() {
        let base = Fix::solid("图层", [0, 0, 4, 6], [1, 2, 3, 255]);
        let with = |change: fn(&mut Fix)| File::new(10, 8, &[base.clone().with(change)], [0; 3]).parse();
        assert!(with(|_| {}).is_ok());

        // 矩形不正常。
        expect_invalid(with(|l| l.rect = [4, 0, 0, 6]), "矩形不正常");
        expect_invalid(with(|l| l.rect = [0, 6, 4, 0]), "矩形不正常");
        expect_invalid(with(|l| l.rect = [i32::MIN, 0, i32::MAX, 6]), "矩形不正常");
        expect_invalid(with(|l| l.rect = [0, 0, 4, 300_001]), "矩形不正常");
        // 矩形和通道数据对不上：数据不够、矩形大得离谱但数据只有一点点。
        expect_invalid(with(|l| l.rect = [0, 0, 4, 7]), "图层“图层”：通道数据的长度和图层的尺寸对不上");
        expect_invalid(with(|l| l.rect = [0, 0, 300_000, 300_000]), "对不上");
        expect_invalid(with(|l| l.rect = [-300_000, 0, 0, 300_000]), "对不上");
        expect_invalid(with(|l| l.channels[1].1.truncate(10)), "对不上");
        expect_invalid(with(|l| l.channels[1].1.truncate(1)), "对不上");
        expect_invalid(with(|l| l.channels[1].1.clear()), "对不上");
        // 通道数据多出来一截不算错。
        assert!(with(|l| l.channels[1].1.extend([7; 9])).is_ok());

        // RLE：行长度表不完整、某一行的数据不够、解出来太长或太短。
        let rle_with = |change: fn(&mut Vec<u8>)| {
            let mut layer = base.clone();
            layer.channels[2].1 = rle(&[5; 24], 6);
            change(&mut layer.channels[2].1);
            File::new(10, 8, &[layer], [0; 3]).parse()
        };
        assert!(rle_with(|_| {}).is_ok());
        expect_invalid(rle_with(|d| d.truncate(2 + 7)), "对不上");
        expect_invalid(rle_with(|d| d.truncate(d.len() - 1)), "RLE 数据不完整");
        expect_invalid(rle_with(|d| d[3] = 200), "RLE 数据不完整");
        expect_invalid(rle_with(|d| d[10] = 0xFA), "RLE 数据解出来比一行还长");
        expect_invalid(rle_with(|d| d[10] = 0xFC), "RLE 数据不完整");
        expect_invalid(rle_with(|d| d[3] = 1), "RLE 数据不完整");
        // 压缩方式。
        expect_invalid(with(|l| l.channels[0].1[1] = 4), "不认识的压缩方式（4）");
        expect_invalid(with(|l| l.channels[0].1[0] = 1), "不认识的压缩方式（256）");
        // 少了颜色通道。
        expect_invalid(with(|l| drop(l.channels.remove(2))), "缺少颜色通道");
        expect_invalid(with(|l| l.channels[3].0 = 7), "缺少颜色通道");

        // 记录本身坏了。
        let patched = |at: usize, bytes: &[u8]| {
            let mut data = File::new(10, 8, std::slice::from_ref(&base), [0; 3]).bytes();
            // 图层信息从"图层与蒙版信息"的长度之后 4 个字节开始：先是 2 个字节的个数，然后是记录。
            let info = data.len() - (2 + 3 * 80) - 4 - layer_info(std::slice::from_ref(&base), 1).len();
            data[info + at..info + at + bytes.len()].copy_from_slice(bytes);
            parse(&data)
        };
        assert!(patched(0, &[0, 1]).is_ok());
        // 个数写多了：多出来的"记录"读到的是后面的像素数据，怎么看都不对。
        expect_invalid(patched(0, &[0, 2]), "第 2 个图层的记录有问题");
        expect_invalid(patched(0, &[0x7F, 0xFF]), "图层个数不正常（32767）");
        expect_invalid(patched(0, &[0x80, 0x00]), "图层个数不正常（32768）");
        expect_invalid(patched(2 + 16, &[0, 65]), "通道太多（65 个）");
        expect_invalid(patched(2 + 16, &[0, 9]), "签名不对");
        expect_invalid(patched(2 + 18 + 24, b"8BIN"), "签名不对");
        // 通道数据的长度写大了，超出文件；写小了，后面的通道就对不上。
        expect_invalid(patched(2 + 18 + 2, &[0x7F, 0xFF, 0xFF, 0xFF]), "图层“图层”的通道数据长度不对，超出了文件的范围");
        expect_invalid(patched(2 + 18 + 2, &[0, 0, 0, 30]), "超出了文件的范围");
        expect_invalid(patched(2 + 18 + 2, &[0, 0, 0, 20]), "不认识的压缩方式（65535）");
        // 附加数据的长度、其中蒙版和混合范围的长度超出范围。
        expect_invalid(patched(2 + 18 + 24 + 12, &[0x00, 0xFF, 0xFF, 0xFF]), "不完整");
        expect_invalid(patched(2 + 18 + 24 + 12, &[0, 0, 0, 3]), "不完整");
        expect_invalid(patched(2 + 18 + 24 + 16, &[0, 0, 0, 200]), "不完整");
        expect_invalid(patched(2 + 18 + 24 + 20, &[0, 0, 0, 200]), "不完整");
        expect_invalid(patched(2 + 18 + 24 + 24, &[250]), "不完整");

        // 不认识的混合模式、ZIP 压缩：不算错，有说明。
        let (doc, notes) = with(|l| l.blend = *b"hue ").unwrap();
        assert_eq!(doc.layers()[0].props.blend, BlendMode::Normal);
        expect_note(&notes, "已改为“正常”：“图层（hue）”");
        let two = [base.clone(), base.clone().with(|l| l.channels[0].1[1] = 2).with(|l| l.name = b"zip".to_vec())];
        let (doc, notes) = File::new(10, 8, &two, [0; 3]).parse().unwrap();
        assert_eq!((doc.layers().len(), doc.layers()[0].props.name.as_str()), (1, "图层"));
        expect_note(&notes, "ZIP 压缩的，本程序读不了，已跳过：“zip”");
    }

    #[test]
    fn odd_but_valid_records_are_read() {
        let base = Fix::solid("x", [0, 0, 2, 2], [255, 255, 255, 255]);
        let names = |layers: &[Fix]| -> Vec<String> {
            let (doc, _) = File::new(4, 4, layers, [0; 3]).parse().unwrap();
            doc.layers().iter().map(|l| l.props.name.clone()).collect()
        };
        // 名字：Pascal 字符串当 UTF-8 读，不是合法 UTF-8 的字节变成替换字符；空名字按位置起名；
        // 各种长度的名字后面补的字节数都对（后面的 luni 块读得到）。
        assert_eq!(names(&[base.clone().with(|l| l.name = "中文".as_bytes().to_vec())]), ["中文"]);
        assert_eq!(names(&[base.clone().with(|l| l.name = vec![b'a', 0xFF])]), ["a\u{FFFD}"]);
        assert_eq!(names(&[base.clone(), base.clone().with(|l| l.name.clear())]), ["x", "图层 2"]);
        for len in 0..9 {
            let layer = base.clone().with(|l| l.name = vec![b'n'; len]).block(b"luni", luni("名字"));
            assert_eq!(names(&[layer]), ["名字"], "Pascal 名字长 {len}");
        }
        // luni：字符数比数据多、数据长度是奇数、空的，都不出事；不成对的代理项变成替换字符。
        assert_eq!(names(&[base.clone().block(b"luni", luni("甲乙").with_count(99))]), ["甲乙"]);
        assert_eq!(names(&[base.clone().block(b"luni", [luni("甲乙"), vec![0x4E]].concat().with_count(3))]), ["甲乙"]);
        assert_eq!(names(&[base.clone().block(b"luni", vec![])]), ["x"]);
        assert_eq!(names(&[base.clone().block(b"luni", vec![0, 0, 0, 0])]), ["x"]);
        assert_eq!(names(&[base.clone().block(b"luni", vec![0, 0, 0, 2, 0xD8, 0x34, 0x00, 0x41])]), ["\u{FFFD}A"]);
        // 附加信息块：长度是奇数、后面补一个字节的（规范的写法）和不补的，补到 4 的倍数的，都能接着往下读。
        let odd = base.clone().block(b"abcd", vec![1, 2, 3]).block(b"luni", luni("后面的"));
        assert_eq!(names(std::slice::from_ref(&odd)), ["后面的"]);
        let unpadded = odd
            .clone()
            .with(|l| l.blocks[0].1 = vec![1, 2])
            .with(|l| l.blocks[0].1.extend(b"8BIMluni\0\0\0\x06\0\0\0\x01\x4E\x2D"));
        assert_eq!(names(&[unpadded.with(|l| drop(l.blocks.pop()))]), ["x"], "块里面的东西不当成块");
        let padded4 =
            base.clone().block(b"abcd", vec![1, 0, 0, 0, 0]).with(|l| l.blocks[0].1.extend([0, 0])).block(b"luni", luni("四"));
        assert_eq!(names(&[padded4]), ["四"]);
        // 长度写得超出范围的块：到此为止，不报错。
        let overlong = base.clone().block(b"luni", luni("好的")).with(|l| l.blocks.push((*b"abcd", vec![0; 4])));
        let mut bytes = File::new(4, 4, &[overlong], [0; 3]).bytes();
        let at = bytes.windows(4).position(|w| w == b"abcd").unwrap();
        bytes[at + 4..at + 8].copy_from_slice(&[0, 0, 0xFF, 0xFF]);
        assert_eq!(parse(&bytes).unwrap().0.layers()[0].props.name, "好的");

        // 图层组的记录不成对（只有组、没有起点；只有起点）也不出事：多出来的组是空文件夹，多出来的起点不算数。
        let group = Fix::empty("组").block(b"lsct", lsct(2));
        let end = Fix::empty("</Layer group>").block(b"lsdk", lsct(3));
        assert_eq!(names(&[base.clone(), group.clone(), group.clone()]), ["x", "组", "组"]);
        assert_eq!(names(&[end.clone(), end.clone(), base.clone()]), ["x"]);
        // 嵌套的组：外层里面依次是一个图层、里层的组（里面一个图层）、又一个图层；外层上面还有一个图层。
        let hidden = group.clone().with(|l| l.flags = 2);
        let faint = group.clone().with(|l| l.opacity = 51);
        let nested = [end.clone(), base.clone(), end.clone(), base.clone(), hidden, base.clone(), faint, base.clone()];
        let (doc, notes) = File::new(4, 4, &nested, [0; 3]).parse().unwrap();
        let props: Vec<_> = doc.layers().iter().map(|l| (l.depth, l.is_folder(), l.props.visible, l.props.opacity)).collect();
        assert_eq!(props, [(1, false, true, 1.0), (2, false, true, 1.0), (1, true, false, 1.0), (1, false, true, 1.0), (0, true, true, 0.2), (0, false, true, 1.0)]);
        assert_eq!(doc.layers()[2].kind, Kind::Folder { open: false });
        assert!(notes.is_empty(), "{notes:?}");
        // 整个在画布之外的图层：离得近的留着（画布 4×4，图层能放到 −4..8），太远的读进来是空的。
        let outside = [base.clone().with(|l| l.rect = [10, 10, 12, 12]), base.clone().with(|l| l.rect = [-2, 0, 0, 2])];
        let (doc, notes) = File::new(4, 4, &outside, [0; 3]).parse().unwrap();
        assert!(doc.layers()[0].tiles.is_empty());
        assert_eq!(content_bounds(&doc.layers()[1].tiles, doc.keep_bounds()), Rect { x0: 0, y0: -2, x1: 2, y1: 0 });
        assert_eq!(notes.len(), 1, "{notes:?}");
        expect_note(&notes, "太远的部分已裁掉：“x”");
    }

    /// 把一个好文件随机改坏几千次：结果可以是报错，也可以是读成了别的样子，但不能崩溃。
    #[test]
    fn mangled_files_never_panic() {
        let tmp = Temp::new("fuzz.psd");
        let mut small = Layer::new("小 a");
        let mut rng = Rng(0x0BAD_5EED);
        for y in 3..20 {
            for x in 60..70 {
                set_pixel(&mut small, x, y, rng.pixel());
            }
        }
        let mut hidden = small.clone();
        hidden.props = LayerProps { visible: false, opacity: 0.3, blend: BlendMode::Screen, name: "b".into(), lock_alpha: true, clip: true, ..Default::default() };
        save(&Document::from_layers(70, 20, [5, 6, 7], vec![small, Layer::new("空"), hidden]), &tmp.0).unwrap();
        let ours = std::fs::read(&tmp.0).unwrap();
        // 再加一个手工拼的、花样多一些的文件。
        let layers = [
            Fix::empty("</Layer group>").block(b"lsct", lsct(3)),
            Fix::solid("m", [-1, -1, 3, 3], [1, 2, 3, 200])
                .with(|l| l.channels.push((-2, raw(&[255; 4]))))
                .block(b"luni", luni("蒙版")),
            Fix::solid("r", [0, 0, 4, 6], [0; 4]).with(|l| l.channels[1].1 = rle(&[5; 24], 6)),
            Fix::empty("g").block(b"lsct", lsct(1)),
        ];
        let theirs = File::new(10, 8, &layers, [1, 2, 3]).bytes();
        assert!(parse(&ours).is_ok() && parse(&theirs).is_ok());

        for good in [&ours, &theirs] {
            for round in 0..6000 {
                let mut bytes = good.to_vec();
                for _ in 0..1 + round % 3 {
                    if bytes.len() < 2 {
                        break;
                    }
                    // 多半改在文件的前一段（文件头和图层记录都在那里）。
                    let range = if round % 4 == 0 { bytes.len() } else { bytes.len().min(700) };
                    let at = rng.next() as usize % range;
                    match rng.next() % 6 {
                        0 => bytes[at] = rng.next() as u8,
                        1 => bytes[at] = [0, 1, 0x7F, 0x80, 0xFF][rng.next() as usize % 5],
                        2 => bytes[at] ^= 1 << (rng.next() % 8),
                        3 => drop(bytes.remove(at)),
                        4 => bytes.insert(at, rng.next() as u8),
                        _ => bytes.truncate(at.max(1)),
                    }
                }
                let _ = parse(&bytes);
            }
        }
    }

    #[test]
    fn saving_over_an_existing_file_replaces_it_and_leaves_no_temp_files() {
        let dir = Temp::new("psd-dir");
        std::fs::create_dir(&dir.0).unwrap();
        let path = dir.0.join(format!("画.{EXTENSION}"));
        save(&Document::new(64, 48), &path).unwrap();
        assert_eq!(load(&path).unwrap().0.width, 64);
        save(&sample_document(), &path).unwrap();
        assert_eq!(load(&path).unwrap().0.layers().len(), 8);
        // 存不成的时候报错，临时文件也收拾掉。这里的目标是个目录，改名顶替不了它。
        let blocked = dir.0.join("挡路的目录");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("里面有东西"), b"x").unwrap();
        assert!(save(&Document::new(8, 8), &blocked).is_err());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
        // 尺寸不对的文档不写。
        let mut bad = Document::new(8, 8);
        bad.width = 0;
        assert_eq!(save(&bad, &dir.0.join("bad.psd")).err().unwrap().kind(), io::ErrorKind::InvalidInput);
        // 图层多到自己都不肯读回来的文档也不写；少一个就可以。
        let crowd = |n: usize| Document::from_layers(8, 8, [0; 3], (0..n).map(|i| Layer::new(&i.to_string())).collect());
        assert_eq!(save(&crowd(MAX_LAYERS), &dir.0.join("crowd.psd")).err().unwrap().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
        save(&crowd(MAX_LAYERS - 1), &dir.0.join("crowd.psd")).unwrap();
        assert_eq!(load(&dir.0.join("crowd.psd")).unwrap().0.layers().len(), MAX_LAYERS - 1);
    }

    /// 和 GIMP 互相读写的核对用的两个辅助"测试"，平时不跑。设好环境变量 `ZHIBI_INTEROP_DIR` 再跑：
    /// `cargo test -p zhibi-core interop -- --ignored --nocapture`
    /// 第一个把示例文档写成 `ours.psd`（文档的内容由 `ora` 模块里的同名测试另存一份供对照）；
    /// 第二个读目录里别的程序写的 `.psd` 文件，把读到的内容存成 `<文件名>.loaded.*`。
    #[test]
    #[ignore]
    fn interop_write_sample() {
        let Some(dir) = interop_dir() else { return };
        save(&sample_document(), &dir.join("ours.psd")).unwrap();
    }

    /// 和上面一样平时不跑：把带文件夹、蒙版、填充图层的示例文档写成 `ours-tree.psd`，
    /// 本程序合成的样子存成 `ours-tree.png`，给 GIMP 读了对照。
    #[test]
    #[ignore]
    fn interop_write_tree() {
        let Some(dir) = interop_dir() else { return };
        let doc = tree_document();
        save(&doc, &dir.join("ours-tree.psd")).unwrap();
        doc.export_png(&dir.join("ours-tree.png")).unwrap();
        // 只有"正常"模式、没有填充图层的一份，用来逐像素比较合成的结果：
        // 底下一个图层；穿透的文件夹里一个半透明、带软边蒙版的图层；隔离的文件夹（不透明度 50%）里两个图层，
        // 其中一个带默认全黑、中间一块白的蒙版。
        let block = |name: &str, depth: u16, rgba: [u16; 4], r: Rect| {
            let mut l = Layer::new(name);
            l.depth = depth;
            let px = to_premultiplied(rgba);
            for y in r.y0..r.y1 {
                for x in r.x0..r.x1 {
                    let t = l.tiles.get_mut((x.div_euclid(TILE), y.div_euclid(TILE)));
                    let i = ((y.rem_euclid(TILE)) * TILE + x.rem_euclid(TILE)) as usize * 4;
                    t.0[i..i + 4].copy_from_slice(&px);
                }
            }
            l
        };
        let full = |v: u8| v as u16 * 257;
        let base = block("底", 0, [full(40), full(160), full(60), 65535], Rect { x0: 0, y0: 0, x1: 140, y1: 100 });
        let mut soft = block("软", 1, [full(230), full(40), full(40), full(180)], Rect { x0: 20, y0: 10, x1: 120, y1: 50 });
        let mut m = Mask { tiles: GrayMap::default(), default: ONE as u16, disabled: false };
        for y in 0..100 {
            for x in 0..140 {
                let v = x as u32 * 255 / 139;
                m.tiles.get_mut((x / TILE, y / TILE), 0)[((y % TILE) * TILE + x % TILE) as usize] = ((v * ONE + 127) / 255) as u16;
            }
        }
        soft.mask = Some(m);
        let mut pass = Layer::folder("穿透");
        pass.props.opacity = 1.0;
        let blue = block("蓝", 1, [full(30), full(60), full(220), 65535], Rect { x0: 50, y0: 40, x1: 130, y1: 95 });
        let mut white = block("白", 1, [65535, 65535, 65535, 65535], Rect { x0: 10, y0: 60, x1: 70, y1: 90 });
        let mut wm = Mask { tiles: GrayMap::default(), default: 0, disabled: false };
        for y in 65..85 {
            for x in 20..60 {
                wm.tiles.get_mut((x / TILE, y / TILE), 0)[((y % TILE) * TILE + x % TILE) as usize] = ONE as u16;
            }
        }
        white.mask = Some(wm);
        let mut isolated = Layer::folder("隔离");
        isolated.props.pass_through = false;
        isolated.props.opacity = 0.5;
        let doc = Document::from_layers(140, 100, [255; 3], vec![base, soft, pass, blue, white, isolated]);
        save(&doc, &dir.join("ours-compare.psd")).unwrap();
        doc.export_png(&dir.join("ours-compare.png")).unwrap();
    }

    #[test]
    #[ignore]
    fn interop_read_foreign() {
        interop_load_all(EXTENSION, load);
    }

    /// 带文件夹、蒙版、填充图层的示例文档（自下而上）：
    /// 文件夹"外"（普通、合上的、正片叠底）里面依次是图层 a（带全白的蒙版）、文件夹"里"（穿透、隐藏，
    /// 里面一个图层 b，带默认全黑、左上一块白的蒙版）；外面是纯色填充（新建的，没有原始描述符，带停用的蒙版）、
    /// 从别处读来的纯色填充（带原始描述符）、读不懂的填充（原始描述符原样保留）。
    pub fn tree_document() -> Document {
        let solid = |name: &str, color: [u8; 3], r: Rect| {
            let mut l = Layer::new(name);
            let px = to_premultiplied([color[0] as u16 * 257, color[1] as u16 * 257, color[2] as u16 * 257, 65535]);
            for y in r.y0..r.y1 {
                for x in r.x0..r.x1 {
                    let t = l.tiles.get_mut((x.div_euclid(TILE), y.div_euclid(TILE)));
                    let i = ((y.rem_euclid(TILE)) * TILE + x.rem_euclid(TILE)) as usize * 4;
                    t.0[i..i + 4].copy_from_slice(&px);
                }
            }
            l
        };
        let mut a = solid("a", [200, 40, 40], Rect { x0: 10, y0: 10, x1: 90, y1: 60 });
        a.depth = 1;
        a.mask = Some(Mask::white());
        let mut b = solid("b", [40, 40, 200], Rect { x0: 40, y0: 30, x1: 130, y1: 90 });
        b.depth = 2;
        let mut mask = Mask { tiles: GrayMap::default(), default: 0, disabled: false };
        for y in 0..70 {
            for x in 0..100 {
                // 8 位表示得了的值，读回来应当一位不差。
                let v = ((x + y) % 256) as u32;
                mask.tiles.get_mut((x / TILE, y / TILE), 0)[((y % TILE) * TILE + x % TILE) as usize] = ((v * ONE + 127) / 255) as u16;
            }
        }
        b.mask = Some(mask);
        let mut inner = Layer::folder("里");
        inner.depth = 1;
        inner.props.visible = false;
        let mut outer = Layer::folder("外");
        outer.kind = Kind::Folder { open: false };
        outer.props.pass_through = false;
        outer.props.blend = BlendMode::Multiply;
        outer.props.opacity = 0.8;
        let mut fresh = Layer::new("纯色 新建");
        fresh.kind = Kind::Fill(Fill { paint: Paint::Solid([12, 200, 99]), psd: None });
        fresh.mask = Some(Mask { tiles: GrayMap::default(), default: 0, disabled: true });
        let mut kept = Layer::new("纯色 原有");
        kept.kind = Kind::Fill(Fill { paint: Paint::Solid([1, 2, 3]), psd: Some((*b"SoCo", solid_color_descriptor([1, 2, 3]))) });
        kept.props.opacity = 0.4;
        let mut other = Layer::new("渐变");
        other.kind = Kind::Fill(Fill { paint: Paint::Other, psd: Some((*b"GdFl", vec![0, 0, 0, 16, 9, 8, 7, 6])) });
        Document::from_layers(140, 100, [255, 250, 240], vec![a, b, inner, outer, fresh, kept, other])
    }

    /// 渐变之类还不会画的填充图层：文件里存着的像素读进来显示，存回去再读，像素还在；
    /// 纯色填充写成铺满画布的像素，不认识填充图层的程序也能看到。
    #[test]
    fn fill_layer_pixels_survive_a_round_trip() {
        let mut other = Layer::new("渐变");
        other.kind = Kind::Fill(Fill { paint: Paint::Other, psd: Some((*b"GdFl", vec![0, 0, 0, 16])) });
        let red = to_premultiplied([65535, 0, 0, 65535]);
        for y in 0..10 {
            for x in 0..10 {
                other.tiles.get_mut((0, 0)).0[(y * TILE + x) as usize * 4..(y * TILE + x) as usize * 4 + 4].copy_from_slice(&red);
            }
        }
        let mut solid = Layer::new("纯色");
        solid.kind = Kind::Fill(Fill { paint: Paint::Solid([10, 200, 30]), psd: None });
        solid.props.visible = false;
        let doc = Document::from_layers(50, 40, [255; 3], vec![other, solid]);
        let tmp = Temp::new("fills.psd");
        save(&doc, &tmp.0).unwrap();
        let (back, _) = load(&tmp.0).unwrap();
        assert_eq!(pixel(&back.layers()[0], 5, 5), red);
        assert_eq!(back.pick(5, 5), doc.pick(5, 5));
        // 纯色那一条记录要写铺满画布的像素（读的时候纯色是现算的，所以直接看要写的记录）。
        let records = plan_records(&doc, doc.bounds());
        let rec = records.iter().find(|r| r.name == "纯色").expect("应当有纯色那一条");
        assert_eq!(rec.rect, doc.bounds());
        assert!(matches!(rec.pixels, Pixels::Color([10, 200, 30])) && rec.flags & FLAG_IRRELEVANT == 0);
    }

    /// 拿 PS 存在文件里的合并图像对照我们合成的样子（只看数字，不存图）：`ZHIBI_PSD_COMPARE` 指着一个 PSD。
    /// 打印差别的分布，和差别大的地方落在画布上哪几块（画布分成 8×8 格）。
    /// `cargo test --release -p zhibi-core compare_with_merged -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn compare_with_merged() {
        let Some(path) = std::env::var_os("ZHIBI_PSD_COMPARE") else { return };
        let data = std::fs::read(&path).unwrap();
        let (doc, notes) = parse(&data).unwrap();
        println!("说明：{notes:#?}");
        let mut r = Bytes(&data);
        r.take(12).unwrap();
        let channels = r.u16().unwrap() as usize;
        r.take(12).unwrap();
        for _ in 0..3 {
            let n = r.u32().unwrap() as usize;
            r.take(n).unwrap();
        }
        let merged = read_merged(r, channels, 1, doc.bounds(), &mut Notes::default()).unwrap();
        let ours = doc.flatten_rgb();
        let (w, h) = (doc.width as usize, doc.height as usize);
        let mut hist = [0usize; 5];
        let mut cells = vec![(0usize, 0u64); 64];
        for y in 0..h {
            for x in 0..w {
                let tc = ((x as i32).div_euclid(TILE), (y as i32).div_euclid(TILE));
                let k = ((y as i32 % TILE) * TILE + x as i32 % TILE) as usize * 4;
                let p = merged.get(tc).map_or([0u16; 4], |t| [t.0[k], t.0[k + 1], t.0[k + 2], t.0[k + 3]]);
                let theirs = crate::ora::shared::to_straight8(p);
                let o = (y * w + x) * 3;
                let d = (0..3).map(|c| ours[o + c].abs_diff(theirs[c])).max().unwrap();
                let bucket = match d {
                    0..=2 => 0,
                    3..=10 => 1,
                    11..=40 => 2,
                    41..=100 => 3,
                    _ => 4,
                };
                hist[bucket] += 1;
                let cell = (y * 8 / h) * 8 + x * 8 / w;
                if d > 10 {
                    cells[cell].0 += 1;
                }
                cells[cell].1 += d as u64;
            }
        }
        let total = (w * h) as f64;
        println!(
            "像素 {}：差 ≤2 {:.2}%，3–10 {:.2}%，11–40 {:.2}%，41–100 {:.2}%，>100 {:.2}%",
            w * h,
            hist[0] as f64 / total * 100.0,
            hist[1] as f64 / total * 100.0,
            hist[2] as f64 / total * 100.0,
            hist[3] as f64 / total * 100.0,
            hist[4] as f64 / total * 100.0
        );
        let per_cell = total / 64.0;
        for row in 0..8 {
            let line: Vec<String> = (0..8).map(|c| format!("{:5.1}", cells[row * 8 + c].0 as f64 / per_cell * 100.0)).collect();
            println!("差 >10 的比例(%)  {}", line.join(" "));
        }
    }

    /// 拿用户文件里 PS 存的合并图像校准图层样式的画法（实现方案 §3.14）：换着几种画法，
    /// 只在带样式的图层附近合成，量和 PS 的差。`ZHIBI_PSD_COMPARE` 指着那个 PSD。
    /// `cargo test --release -p zhibi-core calibrate_styles -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn calibrate_styles() {
        use crate::style::{model, set_model, Model, RangeRule};
        let Some(path) = std::env::var_os("ZHIBI_PSD_COMPARE") else { return };
        let data = std::fs::read(&path).unwrap();
        let (doc, _) = parse(&data).unwrap();
        let mut r = Bytes(&data);
        r.take(12).unwrap();
        let channels = r.u16().unwrap() as usize;
        r.take(12).unwrap();
        for _ in 0..3 {
            let n = r.u32().unwrap() as usize;
            r.take(n).unwrap();
        }
        let merged = read_merged(r, channels, 1, doc.bounds(), &mut Notes::default()).unwrap();
        // 带样式的图层附近（内容的外接矩形四周再加 300 像素）。
        let near = doc.layers().iter().filter(|l| l.style.as_ref().is_some_and(|s| s.shows())).fold(Rect::EMPTY, |r, l| {
            let c = content_bounds(&l.tiles, doc.bounds());
            r.union(Rect { x0: c.x0 - 300, y0: c.y0 - 300, x1: c.x1 + 300, y1: c.y1 + 300 })
        });
        let region = near.intersect(doc.bounds());
        let tiles: Vec<crate::tile::TileCoord> = crate::tile::tiles_in(region).collect();
        let original = model().0;
        let mut variants: Vec<Option<Model>> = vec![None];
        let sweep = std::env::var("ZHIBI_SIGMAS").unwrap_or_else(|_| "0.2,0.25,0.3,0.35".into());
        let rules = match std::env::var("ZHIBI_RANGES").as_deref() {
            Ok("all") => vec![RangeRule::Ignore, RangeRule::Scale, RangeRule::Shift],
            _ => vec![RangeRule::Scale],
        };
        for sigma in sweep.split(',').filter_map(|v| v.trim().parse::<f32>().ok()) {
            for &range in &rules {
                variants.push(Some(Model { sigma_per_size: sigma, range }));
            }
        }
        // 投影那一片（下半）和发光那一片（上半）分开量。
        let split_y = 3000;
        for m in variants {
            // None：不画样式，作对照。
            let styled: Vec<_> = doc.layers().iter().map(|l| l.style.clone()).collect();
            let mut doc = Document::from_layers(doc.width, doc.height, doc.background, doc.layers().to_vec());
            match m {
                Some(m) => set_model(m),
                None => {
                    let layers: Vec<Layer> = doc.layers().iter().zip(&styled).map(|(l, _)| Layer { style: None, ..l.clone() }).collect();
                    doc = Document::from_layers(doc.width, doc.height, doc.background, layers);
                }
            }
            let doc = &doc;
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
            let errs: Vec<([f64; 2], [usize; 2], [usize; 2])> = std::thread::scope(|s| {
                let handles: Vec<_> = tiles
                    .chunks(tiles.len().div_ceil(threads).max(1))
                    .map(|chunk| {
                        let merged = &merged;
                        s.spawn(move || {
                            let mut out = vec![0u32; (TILE * TILE) as usize];
                            let (mut sum, mut big, mut count) = ([0f64; 2], [0usize; 2], [0usize; 2]);
                            for &tc in chunk {
                                doc.composite_tile(None, tc, &mut out);
                                let r = Rect { x0: tc.0 * TILE, y0: tc.1 * TILE, x1: (tc.0 + 1) * TILE, y1: (tc.1 + 1) * TILE }.intersect(region);
                                for y in r.y0..r.y1 {
                                    for x in r.x0..r.x1 {
                                        let k = ((y - tc.1 * TILE) * TILE + (x - tc.0 * TILE)) as usize;
                                        let p = merged.get(tc).map_or([0u16; 4], |t| [t.0[k * 4], t.0[k * 4 + 1], t.0[k * 4 + 2], t.0[k * 4 + 3]]);
                                        let theirs = crate::ora::shared::to_straight8(p);
                                        let v = out[k];
                                        let ours = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
                                        let d = (0..3).map(|c| ours[c].abs_diff(theirs[c])).max().unwrap();
                                        let part = (y >= split_y) as usize;
                                        sum[part] += d as f64;
                                        big[part] += (d > 10) as usize;
                                        count[part] += 1;
                                    }
                                }
                            }
                            (sum, big, count)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for (part, name) in [(1, "投影那一片"), (0, "发光那一片")] {
                let (sum, big, count) = errs.iter().fold((0.0, 0, 0), |a, e| (a.0 + e.0[part], a.1 + e.1[part], a.2 + e.2[part]));
                println!("{m:?} {name}：平均差 {:.3}，差 >10 的 {:.3}%", sum / count.max(1) as f64, big as f64 / count.max(1) as f64 * 100.0);
            }
        }
        set_model(original);
    }

    // ---- 图层样式（自己拼的描述符，结构照 Adobe 公开的 PSD 规范） ----

    /// 测试里拼描述符用的值。
    pub(crate) enum V {
        Bool(bool),
        /// 带单位的数（`#Prc`、`#Pxl`、`#Ang`）
        Unit(&'static [u8; 4], f64),
        Doub(f64),
        Enum(&'static [u8], &'static [u8]),
        Obj(&'static [u8], Vec<(&'static [u8], V)>),
        List(Vec<V>),
    }

    fn put_id(out: &mut Vec<u8>, key: &[u8]) {
        let n = if key.len() == 4 { 0 } else { key.len() as u32 };
        out.extend(n.to_be_bytes());
        out.extend(key);
    }

    fn put_value(out: &mut Vec<u8>, v: &V) {
        match v {
            V::Bool(b) => {
                out.extend(b"bool");
                out.push(*b as u8);
            }
            V::Unit(unit, x) => {
                out.extend(b"UntF");
                out.extend(*unit);
                out.extend(x.to_bits().to_be_bytes());
            }
            V::Doub(x) => {
                out.extend(b"doub");
                out.extend(x.to_bits().to_be_bytes());
            }
            V::Enum(t, val) => {
                out.extend(b"enum");
                put_id(out, t);
                put_id(out, val);
            }
            V::Obj(class, items) => {
                out.extend(b"Objc");
                put_descriptor(out, class, items);
            }
            V::List(items) => {
                out.extend(b"VlLs");
                out.extend((items.len() as u32).to_be_bytes());
                for item in items {
                    put_value(out, item);
                }
            }
        }
    }

    fn put_descriptor(out: &mut Vec<u8>, class: &[u8], items: &[(&[u8], V)]) {
        out.extend(0u32.to_be_bytes()); // 名字：空的 Unicode 字符串
        put_id(out, class);
        out.extend((items.len() as u32).to_be_bytes());
        for (k, v) in items {
            put_id(out, k);
            put_value(out, v);
        }
    }

    /// 一个 `lfx2` 块：版本 0、描述符版本 16、类为 `null` 的描述符。
    pub(crate) fn lfx2(items: Vec<(&'static [u8], V)>) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 0, 0, 0, 0, 16];
        put_descriptor(&mut out, b"null", &items);
        out
    }

    fn color(r: f64, g: f64, b: f64) -> V {
        V::Obj(b"RGBC", vec![(b"Rd  ", V::Doub(r)), (b"Grn ", V::Doub(g)), (b"Bl  ", V::Doub(b))])
    }

    /// 外发光：橙色、滤色、不透明度 60%、扩展 10%、大小 8、范围 50%。
    pub(crate) fn outer_glow() -> V {
        V::Obj(
            b"OrGl",
            vec![
                (b"enab", V::Bool(true)),
                (b"present", V::Bool(true)),
                (b"Md  ", V::Enum(b"BlnM", b"Scrn")),
                (b"Clr ", color(240.0, 115.0, 24.0)),
                (b"Opct", V::Unit(b"#Prc", 60.0)),
                (b"GlwT", V::Enum(b"BETE", b"SfBL")),
                (b"Ckmt", V::Unit(b"#Pxl", 10.0)),
                (b"blur", V::Unit(b"#Pxl", 8.0)),
                (b"Nose", V::Unit(b"#Prc", 0.0)),
                (b"Inpr", V::Unit(b"#Prc", 50.0)),
            ],
        )
    }

    /// 一个只有外发光的样式，效果缩放 `scale`%。
    pub(crate) fn glow_style(scale: f64) -> Style {
        let block = lfx2(vec![(b"Scl ", V::Unit(b"#Prc", scale)), (b"masterFXSwitch", V::Bool(true)), (b"OrGl", outer_glow())]);
        parse_style(&[(*b"lfx2", block.as_slice())], None).0
    }

    #[test]
    fn layer_styles_are_read_from_the_descriptor() {
        let shadow = V::Obj(
            b"DrSh",
            vec![
                (b"enab", V::Bool(true)),
                (b"Md  ", V::Enum(b"BlnM", b"Mltp")),
                (b"Clr ", color(87.0, 87.0, 87.0)),
                (b"Opct", V::Unit(b"#Prc", 86.0)),
                (b"uglg", V::Bool(true)),
                (b"lagl", V::Unit(b"#Ang", 87.0)),
                (b"Dstn", V::Unit(b"#Pxl", 8.0)),
                (b"Ckmt", V::Unit(b"#Pxl", 0.0)),
                (b"blur", V::Unit(b"#Pxl", 9.0)),
                (b"layerConceals", V::Bool(true)),
            ],
        );
        let inner = V::Obj(
            b"IrGl",
            vec![
                (b"enab", V::Bool(true)),
                (b"Md  ", V::Enum(b"BlnM", b"Scrn")),
                (b"Clr ", color(221.0, 148.0, 22.0)),
                (b"Opct", V::Unit(b"#Prc", 51.0)),
                (b"Ckmt", V::Unit(b"#Pxl", 15.0)),
                (b"blur", V::Unit(b"#Pxl", 95.0)),
                (b"Inpr", V::Unit(b"#Prc", 40.0)),
                (b"glwS", V::Enum(b"IGSr", b"SrcE")),
                // 不是直线的等高线：只能近似。
                (b"TrnS", V::Obj(b"ShpC", vec![(b"Crv ", V::List(vec![V::Obj(b"CrPt", vec![(b"Hrzn", V::Doub(0.0)), (b"Vrtc", V::Doub(0.0))]), V::Obj(b"CrPt", vec![(b"Hrzn", V::Doub(128.0)), (b"Vrtc", V::Doub(200.0))]), V::Obj(b"CrPt", vec![(b"Hrzn", V::Doub(255.0)), (b"Vrtc", V::Doub(255.0))])]))])),
            ],
        );
        let bevel = V::Obj(b"ebbl", vec![(b"enab", V::Bool(true))]);
        let off = V::Obj(b"FrFX", vec![(b"enab", V::Bool(false))]);
        let block = lfx2(vec![
            (b"Scl ", V::Unit(b"#Prc", 200.0)),
            (b"masterFXSwitch", V::Bool(true)),
            (b"DrSh", shadow),
            (b"IrGl", inner),
            (b"ebbl", bevel),
            (b"FrFX", off),
        ]);
        let (mut style, issues) = parse_style(&[(*b"lrFX", &[0u8; 6][..]), (*b"lfx2", block.as_slice())], Some((30, 40)));
        assert!(issues.len() == 2 && issues.contains(&StyleIssue::Unsupported) && issues.contains(&StyleIssue::Approximate), "{issues:?}");
        assert_eq!((style.enabled, style.light, style.effects.len()), (true, Some((30, 40)), 2));
        // 用全局光：角度是 30°，不是 87°；距离、大小乘了 200%。
        let EffectKind::DropShadow { dx, dy, knockout } = style.effects[0].kind else { panic!() };
        assert!((dx + 16.0 * 30f32.to_radians().cos()).abs() < 1e-3 && (dy - 16.0 * 30f32.to_radians().sin()).abs() < 1e-3 && knockout);
        let e = &style.effects[0];
        assert_eq!((e.color, e.blend, e.size), ([87; 3], BlendMode::Multiply, 18.0));
        assert!((e.opacity - 0.86).abs() < 1e-6);
        let g = &style.effects[1];
        assert_eq!((g.kind, g.color, g.blend, g.size), (EffectKind::InnerGlow { center: false }, [221, 148, 22], BlendMode::Screen, 190.0));
        assert!((g.spread - 0.15).abs() < 1e-6 && (g.range - 0.4).abs() < 1e-6);
        // 关掉总开关：原始块里那个字节跟着变，再读一遍还是关着的；块的长度不变。
        style.set_enabled(false);
        let (_, d) = &style.psd[1];
        assert_eq!(d.len(), block.len());
        let again = parse_style(&[(*b"lfx2", d.as_slice())], None).0;
        assert!(!again.enabled && again.effects.len() == 2);
        // 读不懂的块：原样留着。
        let (bad, issues) = parse_style(&[(*b"lfx2", &[1u8, 2, 3][..])], None);
        assert_eq!((issues, bad.psd.len(), bad.effects.len()), (vec![StyleIssue::Unreadable], 1, 0));
    }

    /// 一个铺着白色方块的图层，带外发光。
    fn glowing_square() -> Document {
        let mut l = Layer::new("发光");
        for y in 30..70 {
            for x in 30..70 {
                let t = l.tiles.get_mut((x / TILE, y / TILE));
                let k = ((y % TILE) * TILE + x % TILE) as usize * 4;
                t.0[k..k + 4].copy_from_slice(&[ONE as u16; 4]);
            }
        }
        l.style = Some(glow_style(100.0));
        // 底下铺满一层不透明的灰：发光是"滤色"，合并时要有不透明的东西垫着，画面才不变（PS 也是这样）。
        let mut bottom = Layer::new("底");
        let grey = to_premultiplied([40 * 257, 40 * 257, 40 * 257, 65535]);
        for tc in crate::tile::tiles_in(Rect { x0: 0, y0: 0, x1: 100, y1: 100 }) {
            for k in 0..(TILE * TILE) as usize {
                bottom.tiles.get_mut(tc).0[k * 4..k * 4 + 4].copy_from_slice(&grey);
            }
        }
        Document::from_layers(100, 100, [255, 255, 255], vec![bottom, l])
    }

    #[test]
    fn styles_show_toggle_and_survive_saving() {
        let mut doc = glowing_square();
        let plain = [40u8; 3];
        // 外发光：方块外面紧挨着的地方被染成橙色，远处不变。
        let near = doc.pick(28, 50).unwrap();
        assert!(near[0] > 80 && near[0] > near[2], "{near:?}");
        assert_eq!(doc.pick(5, 5), Some(plain));
        // 关掉：和没有样式一样；撤销：又回来了。
        doc.set_active(1);
        assert!(doc.toggle_style());
        assert_eq!(doc.pick(28, 50), Some(plain));
        doc.undo();
        assert_eq!(doc.pick(28, 50), Some(near));

        // 存成 PSD 再读：块一个字节不差，效果一样，关着的还是关着的。
        doc.toggle_style();
        let tmp = Temp::new("style.psd");
        save(&doc, &tmp.0).unwrap();
        let (back, notes) = load(&tmp.0).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let a = doc.layers()[1].style.clone().unwrap();
        let b = back.layers()[1].style.as_ref().unwrap();
        assert_eq!((&a.psd, &a.effects, a.enabled), (&b.psd, &b.effects, b.enabled));
        assert!(!b.enabled);
        assert_eq!(back.pick(28, 50), Some(plain));
        // 原生格式也一样（这回开着）。
        doc.toggle_style();
        let tmp = Temp::new("style.zhibi");
        crate::format::save(&doc, &tmp.0).unwrap();
        let back = crate::format::load(&tmp.0).unwrap();
        let (a, b) = (doc.layers()[1].style.as_ref().unwrap(), back.layers()[1].style.as_ref().unwrap());
        assert_eq!((&a.psd, &a.effects, a.enabled), (&b.psd, &b.effects, true));
        assert_eq!(back.pick(28, 50), Some(near));
    }

    /// 用户文件里带样式的图层，各自算一遍效果要多久（`ZHIBI_PSD_COMPARE` 指着那个 PSD）。
    #[test]
    #[ignore]
    fn style_timing() {
        let Some(path) = std::env::var_os("ZHIBI_PSD_COMPARE") else { return };
        let (doc, _) = load(std::path::Path::new(&path)).unwrap();
        for l in doc.layers().iter().filter(|l| l.style.as_ref().is_some_and(|s| s.shows())) {
            let t = std::time::Instant::now();
            let p = crate::style::render(&l.tiles, l.mask.as_ref(), &l.style.as_ref().unwrap().effects, doc.bounds(), &crate::style::PS_MODEL);
            println!("{}：{} 个效果，{:.0} 毫秒", l.props.name, p.fx.len(), t.elapsed().as_secs_f64() * 1000.0);
        }
    }

    /// 给窗口测试用：`ZHIBI_INTEROP_DIR` 里写一个带外发光图层的 PSD（`style.psd`）。
    #[test]
    #[ignore]
    fn write_style_window_doc() {
        let Some(dir) = interop_dir() else { return };
        save(&glowing_square(), &dir.join("style.psd")).unwrap();
    }

    #[test]
    fn merging_down_paints_the_style_in() {
        let mut doc = glowing_square();
        let before: Vec<_> = (0..100).flat_map(|y| (0..100).map(move |x| (x, y))).map(|(x, y)| doc.pick(x, y)).collect();
        doc.set_active(1);
        assert!(doc.merge_down());
        assert_eq!(doc.layers().len(), 1);
        assert!(doc.layers()[0].style.is_none());
        let after: Vec<_> = (0..100).flat_map(|y| (0..100).map(move |x| (x, y))).map(|(x, y)| doc.pick(x, y)).collect();
        assert_eq!(before, after);
        doc.undo();
        assert!(doc.layers()[1].style.is_some());
    }

    #[test]
    fn folders_masks_and_fills_round_trip() {
        let doc = tree_document();
        let tmp = Temp::new("tree.psd");
        save(&doc, &tmp.0).unwrap();
        let (back, notes) = load(&tmp.0).unwrap();
        // 读不懂的那个填充图层照实说明，别的什么都不用说。
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(back.layers().len(), doc.layers().len());
        for (x, y) in doc.layers().iter().zip(back.layers()) {
            let name = &x.props.name;
            // 新建的纯色填充读回来带着我们写的描述符，所以只比画的是什么。
            let paint = |k: &Kind| match k {
                Kind::Fill(f) => Kind::Fill(Fill { paint: f.paint.clone(), psd: None }),
                other => other.clone(),
            };
            assert_eq!((&y.props.name, y.depth, paint(&y.kind)), (name, x.depth, paint(&x.kind)), "{name}");
            assert_eq!((y.props.visible, y.props.blend, y.props.pass_through, y.props.clip), (x.props.visible, x.props.blend, x.props.pass_through, x.props.clip), "{name}");
            assert!((y.props.opacity - x.props.opacity).abs() <= 0.5 / 255.0, "{name}");
            assert_eq!(x.mask.as_ref().map(|m| (m.default, m.disabled)), y.mask.as_ref().map(|m| (m.default, m.disabled)), "{name}");
            if let (Some(m), Some(n)) = (&x.mask, &y.mask) {
                for yy in 0..doc.height {
                    for xx in 0..doc.width {
                        let at = |m: &Mask| m.tiles.get((xx / TILE, yy / TILE)).map_or(m.default, |t| t[((yy % TILE) * TILE + xx % TILE) as usize]);
                        assert_eq!(at(m), at(n), "{name} 的蒙版 ({xx}, {yy})");
                    }
                }
            }
            for yy in 0..doc.height {
                for xx in 0..doc.width {
                    assert_eq!(pixel(x, xx, yy), pixel(y, xx, yy), "{name} ({xx}, {yy})");
                }
            }
        }
        // 原始描述符原样回来（后面可能多了补齐用的 0）。
        for i in [5, 6] {
            let (Kind::Fill(a), Kind::Fill(b)) = (&doc.layers()[i].kind, &back.layers()[i].kind) else { panic!() };
            let ((ka, da), (kb, db)) = (a.psd.as_ref().unwrap(), b.psd.as_ref().unwrap());
            assert_eq!(ka, kb);
            assert!(db.starts_with(da) && db[da.len()..].iter().all(|&v| v == 0), "{i}");
        }
        assert_eq!(back.flatten_rgb(), doc.flatten_rgb());
        // 存一次读一次再存，文件逐字节不变。
        let again = Temp::new("tree-again.psd");
        save(&back, &again.0).unwrap();
        assert_eq!(std::fs::read(&tmp.0).unwrap(), std::fs::read(&again.0).unwrap());
    }

    #[test]
    fn solid_color_descriptors_in_other_color_models() {
        // 照规范拼几个不同颜色模型的纯色描述符。
        let descriptor = |class: &[u8; 4], items: &[(&[u8; 4], f64)]| {
            let mut d = Vec::new();
            let id = |d: &mut Vec<u8>, k: &[u8; 4]| {
                d.extend(0u32.to_be_bytes());
                d.extend(k);
            };
            d.extend(16u32.to_be_bytes());
            d.extend([0, 0, 0, 1, 0, 0]);
            id(&mut d, b"null");
            d.extend(1u32.to_be_bytes());
            id(&mut d, b"Clr ");
            d.extend(b"Objc");
            d.extend([0, 0, 0, 1, 0, 0]);
            id(&mut d, class);
            d.extend((items.len() as u32).to_be_bytes());
            for (k, v) in items {
                id(&mut d, k);
                if **k == *b"H   " {
                    d.extend(b"UntF#Ang");
                } else {
                    d.extend(b"doub");
                }
                d.extend(v.to_bits().to_be_bytes());
            }
            d
        };
        assert_eq!(solid_color(&descriptor(b"RGBC", &[(b"Rd  ", 255.0), (b"Grn ", 127.6), (b"Bl  ", 0.0)])), Some([255, 128, 0]));
        assert_eq!(solid_color(&descriptor(b"HSBC", &[(b"H   ", 120.0), (b"Strt", 100.0), (b"Brgh", 50.0)])), Some([0, 128, 0]));
        assert_eq!(solid_color(&descriptor(b"Grsc", &[(b"Gry ", 25.0)])), Some([191; 3]));
        assert_eq!(solid_color(&descriptor(b"CMYC", &[(b"Cyn ", 0.0), (b"Mgnt", 100.0), (b"Ylw ", 100.0), (b"Blck", 0.0)])), Some([255, 0, 0]));
        let white = solid_color(&descriptor(b"LbCl", &[(b"Lmnc", 100.0), (b"A   ", 0.0), (b"B   ", 0.0)])).unwrap();
        assert!(white.iter().all(|&c| c >= 254), "{white:?}");
        // 少了一项、不认识的颜色模型、截断的数据：读不出来，但不出事。
        assert_eq!(solid_color(&descriptor(b"RGBC", &[(b"Rd  ", 1.0)])), None);
        assert_eq!(solid_color(&descriptor(b"XXXX", &[])), None);
        let full = descriptor(b"RGBC", &[(b"Rd  ", 1.0), (b"Grn ", 2.0), (b"Bl  ", 3.0)]);
        for n in 0..full.len() {
            assert_eq!(solid_color(&full[..n]), None, "{n}");
        }
        assert_eq!(solid_color(&solid_color_descriptor([9, 99, 199])), Some([9, 99, 199]));
    }
}
