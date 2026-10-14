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
//! 读的时候，PSD 里本程序没有的东西（图层组、蒙版、调整图层、图层样式……）要么展开、要么忽略，
//! 并且在返回的说明里一一交代，见 [`load`]。

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::document::{BlendMode, Document, Layer, LayerProps, MAX_SIDE};
use crate::lang::tr;
use crate::ora::shared::{
    self, background_name, check_document, content_bounds, invalid, read_row, save_atomically, store_row, take_background,
    Notes, MAX_LAYERS,
};
use crate::tile::TileMap;
use crate::trf;
use crate::Rect;

/// 文件扩展名（不含点）
pub const EXTENSION: &str = "psd";

const SIGNATURE: &[u8; 4] = b"8BPS";
/// 图像资源块、混合模式、附加图层信息块共用的签名。
const BLOCK_SIGNATURE: &[u8; 4] = b"8BIM";
/// 文件头里"颜色模式"一项的值：RGB。
const MODE_RGB: u16 = 3;
/// 图像资源的编号：分辨率（ResolutionInfo）。
const RESOURCE_RESOLUTION: u16 = 1005;
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
    put_u32(&mut front, 12 + resource.len() as u32);
    front.extend(BLOCK_SIGNATURE);
    put_u16(&mut front, RESOURCE_RESOLUTION);
    put_u16(&mut front, 0); // 资源的名字：空的 Pascal 字符串，补成偶数长度
    put_u32(&mut front, resource.len() as u32);
    front.extend(resource);

    // 图层与蒙版信息。最底下多写一个图层，代表画布的背景色。
    let background = LayerProps { name: background_name().into(), ..Default::default() };
    let rects: Vec<Rect> = doc.layers().iter().map(|l| content_bounds(&l.tiles, canvas)).collect();
    let section_at = front.len();
    put_u32(&mut front, 0); // 这一整段的长度，后填
    put_u32(&mut front, 0); // 其中"图层信息"的长度，后填
    let info_start = front.len();
    put_u16(&mut front, (doc.layers().len() + 1) as u16);
    let mut lengths_at = vec![push_record(&mut front, &background, canvas)];
    for (layer, r) in doc.layers().iter().zip(&rects) {
        lengths_at.push(push_record(&mut front, &layer.props, *r));
    }

    let mut out = BufWriter::new(file);
    out.write_all(&front)?;
    let mut info_len = (front.len() - info_start) as u64;
    let [r, g, b] = doc.background;
    let solid = encode_channels(canvas, |_, row| row.as_chunks_mut::<4>().0.fill([r, g, b, 255]))?;
    let layers = doc
        .layers()
        .iter()
        .zip(&rects)
        .map(|(layer, r)| encode_channels(*r, |y, row| read_row(&layer.tiles, y, r.x0, r.x1, row)));
    for (channels, lengths_at) in std::iter::once(Ok(solid)).chain(layers).zip(lengths_at) {
        for (k, data) in channels?.iter().enumerate() {
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

/// 往 `out` 里加一条图层记录。各通道的"数据长度"先写 0，返回第一个通道的这个字段的位置
/// （后面每个通道的依次往后 6 个字节）。
fn push_record(out: &mut Vec<u8>, props: &LayerProps, rect: Rect) -> usize {
    // 矩形：上、左、下、右。
    for v in [rect.y0, rect.x0, rect.y1, rect.x1] {
        out.extend(v.to_be_bytes());
    }
    put_u16(out, CHANNELS.len() as u16);
    let lengths_at = out.len() + 2;
    for (id, _) in CHANNELS {
        out.extend(id.to_be_bytes());
        put_u32(out, 0);
    }
    out.extend(BLOCK_SIGNATURE);
    out.extend(blend_key(props.blend));
    // 合成时非数当 0 用，这里保持一致。
    let opacity = if props.opacity.is_nan() { 0.0 } else { props.opacity.clamp(0.0, 1.0) };
    out.push((opacity * 255.0).round() as u8);
    out.push(props.clip as u8); // 剪贴：0 是普通图层，1 是剪贴在下面那一层上
    let flags = FLAG_BIT4_VALID | if props.visible { 0 } else { FLAG_HIDDEN } | if props.lock_alpha { FLAG_KEEP_ALPHA } else { 0 };
    out.push(flags);
    out.push(0); // 填充字节

    let mut extra = Vec::new();
    // 图层蒙版数据：没有。
    put_u32(&mut extra, 0);
    // 混合范围："合成灰度"和四个通道各一组，每组是源和目标的范围，都取全范围（黑 0、0，白 255、255）。
    put_u32(&mut extra, 5 * 8);
    for _ in 0..5 * 2 {
        extra.extend([0, 0, 255, 255]);
    }
    // 图层名，Pascal 字符串（一个字节的长度加内容），连长度字节一起补到 4 的倍数。它最多 255 个字节，
    // 编码也没有定准，现在的程序都以后面的 Unicode 名字为准；这里放的是 UTF-8，太长就在字符的边界上截断。
    let mut end = props.name.len().min(255);
    while !props.name.is_char_boundary(end) {
        end -= 1;
    }
    extra.push(end as u8);
    extra.extend(&props.name.as_bytes()[..end]);
    extra.resize(extra.len().next_multiple_of(4), 0);
    // 附加信息块 `luni`：完整的图层名。四个字节的字符数（UTF-16 的码元数），然后是 UTF-16 的字符；
    // 整块补到 4 的倍数。
    let units: Vec<u16> = props.name.encode_utf16().collect();
    let len = (4 + units.len() * 2).next_multiple_of(4);
    extra.extend(BLOCK_SIGNATURE);
    extra.extend(b"luni");
    put_u32(&mut extra, len as u32);
    put_u32(&mut extra, units.len() as u32);
    extra.extend(units.iter().flat_map(|u| u.to_be_bytes()));
    extra.resize(extra.len().next_multiple_of(4), 0);

    put_u32(out, extra.len() as u32);
    out.extend(extra);
    lengths_at
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
/// - 图层组展开成普通图层，组的可见性和不透明度并进组里的每个图层，组自己的混合模式不管；
/// - 图层蒙版、矢量蒙版、图层样式忽略；剪贴和"保护不透明度"两个开关照原样读入；
/// - 文字图层、智能对象只留下文件里存着的像素；
/// - 调整图层、没有像素的填充图层跳过；文件里标明"像素与外观无关"的图层不读像素；
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

    // 2. 颜色模式数据，用不上。
    let n = r.length()?;
    r.take(n)?;
    // 3. 图像资源。
    let n = r.length()?;
    let dpi = read_resolution(r.sub(n)?).unwrap_or(DEFAULT_DPI);
    // 4. 图层与蒙版信息。
    let n = r.length()?;
    let records = read_layer_section(r.sub(n)?)?;

    let mut layers: Vec<Layer> = Vec::new();
    // 还没遇到组记录的那些图层组各自是从第几个图层开始的。
    let mut group_starts: Vec<usize> = Vec::new();
    let mut groups = 0;
    for record in &records {
        // 图层是自下而上排的：先是标出组的起点的隐藏记录，然后是组里的图层，最后才是组本身。
        match record.section {
            SECTION_END => {
                group_starts.push(layers.len());
                continue;
            }
            SECTION_OPEN | SECTION_CLOSED => {
                groups += 1;
                let start = group_starts.pop().unwrap_or(layers.len());
                for layer in &mut layers[start..] {
                    layer.props.visible &= !record.hidden;
                    layer.props.opacity *= record.opacity as f32 / 255.0;
                }
                continue;
            }
            _ => {}
        }
        let name = record.name.as_str();
        if record.adjustment {
            notes.layer(
                tr("下列调整图层本程序没有对应的功能，已跳过", "Skipped adjustment layers, which this program does not support"),
                name,
            );
            continue;
        }
        // 标明"像素与外观无关"的图层，文件里即使存着像素也不能用（规范说那是未定义的），当作没有。
        let rect = if record.irrelevant { Rect::EMPTY } else { record.rect };
        let mut tiles = TileMap::default();
        if rect.is_empty() {
            if record.fill {
                notes.layer(
                    tr(
                        "下列填充图层（纯色、渐变或图案）没有像素，已跳过",
                        "Skipped fill layers (solid color, gradient or pattern) that have no pixels",
                    ),
                    name,
                );
                continue;
            }
            if !record.rect.is_empty() {
                notes.layer(
                    tr(
                        "下列图层的像素在文件里标明了与画面无关，没有读入",
                        "Did not read the pixels of these layers, which the file marks as not affecting the image",
                    ),
                    name,
                );
            }
        } else {
            let outcome = read_layer_pixels(record, sample_bytes, canvas, &mut tiles)
                .map_err(|e| io::Error::new(e.kind(), trf!("图层“{name:.80}”：{e}", "Layer \"{name:.80}\": {e}")))?;
            if let Pixels::Zip = outcome {
                notes.layer(
                    tr(
                        "下列图层的像素是用 ZIP 压缩的，本程序读不了，已跳过",
                        "Skipped layers with ZIP-compressed pixels, which this program cannot read",
                    ),
                    name,
                );
                continue;
            }
            if rect.intersect(canvas) != rect {
                notes.layer(shared::clipped_kind(), name);
            }
            if record.fill {
                notes.layer(tr("下列填充图层只读入了文件里存着的像素", "Read only the stored pixels of these fill layers"), name);
            }
        }
        if record.mask {
            notes.layer(tr("下列图层的蒙版被忽略了", "Ignored the masks of these layers"), name);
        }
        if record.effects {
            notes.layer(tr("下列图层的图层样式（效果）被忽略了", "Ignored the layer styles (effects) of these layers"), name);
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
        let blend = blend_of(&record.blend).unwrap_or_else(|| {
            let key = String::from_utf8_lossy(&record.blend);
            let key = key.trim_end();
            notes.layer(shared::unknown_blend_kind(), &trf!("{name}（{key}）", "{name} ({key})"));
            BlendMode::Normal
        });
        let props = LayerProps {
            name: name.to_string(),
            opacity: record.opacity as f32 / 255.0,
            visible: !record.hidden,
            blend,
            lock_alpha: record.keep_alpha,
            clip: record.clipping,
            ..Default::default()
        };
        layers.push(Layer { props, tiles });
    }
    if groups > 0 {
        notes.add(shared::groups_note(groups));
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
        layers.push(Layer { props, tiles });
    }

    let background = take_background(&mut layers, canvas).unwrap_or([255; 3]);
    let mut doc = Document::from_layers(canvas.x1, canvas.y1, background, layers);
    doc.dpi = dpi;
    Ok((doc, notes.into_vec()))
}

/// 在图像资源里找分辨率。资源块的结构：签名、编号、名字（Pascal 字符串，补成偶数长度）、
/// 数据长度、数据（补成偶数长度）。找不到或者资源坏了就返回 `None`，这只是附带的信息。
fn read_resolution(mut r: Bytes) -> Option<u32> {
    while r.len() > 0 {
        r.take(4).ok()?;
        let id = r.u16().ok()?;
        let name_len = r.u8().ok()? as usize;
        r.take(name_len + (name_len + 1) % 2).ok()?;
        let len = r.length().ok()?;
        let mut data = r.sub(len).ok()?;
        if len % 2 == 1 {
            let _ = r.take(1);
        }
        if id == RESOURCE_RESOLUTION {
            // 横向分辨率，16.16 定点数。不管显示单位选的是什么，存的都是像素/英寸。
            let dpi = (data.u32().ok()? as u64 + 0x8000) >> 16;
            return (dpi >= 1).then_some(dpi as u32);
        }
    }
    None
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
    /// 有图层蒙版或矢量蒙版
    mask: bool,
    /// 有图层样式
    effects: bool,
    /// 文字图层
    text: bool,
    /// 智能对象
    smart: bool,
    /// 调整图层
    adjustment: bool,
    /// 填充图层（纯色、渐变、图案）
    fill: bool,
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
    let mut mask = n > 0;
    extra.take(n)?;
    let n = extra.length()?;
    extra.take(n)?;
    // 名字是 Pascal 字符串，连长度字节一起补到 4 的倍数。编码没有定准，先当 UTF-8 读；
    // 后面有 Unicode 名字（`luni` 块）的话以它为准。
    let n = extra.u8()? as usize;
    let mut name = String::from_utf8_lossy(extra.take(n)?).into_owned();
    let padding = (n + 1).next_multiple_of(4) - (n + 1);
    extra.take(padding.min(extra.len()))?;

    mask |= channels.iter().any(|c| c.id < -1);
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
        mask,
        effects: false,
        text: false,
        smart: false,
        adjustment: false,
        fill: false,
    };
    for (key, data) in Blocks(extra) {
        match &key {
            b"luni" => {
                if let Some(unicode) = read_unicode_name(Bytes(data)).filter(|n| !n.is_empty()) {
                    name = unicode;
                }
            }
            // 图层组的分隔记录。嵌套很深的组用的是 `lsdk`，内容一样。
            b"lsct" | b"lsdk" => record.section = Bytes(data).u32().unwrap_or(0),
            b"vmsk" | b"vsms" => record.mask = true,
            b"lrFX" | b"lfx2" | b"lmfx" => record.effects = true,
            b"TySh" | b"tySh" => record.text = true,
            b"SoLd" | b"SoLE" | b"PlLd" | b"plLd" => record.smart = true,
            b"SoCo" | b"GdFl" | b"PtFl" => record.fill = true,
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

enum Pixels {
    Read,
    /// 像素是用 ZIP 压缩的，没读
    Zip,
}

/// 把一个图层的像素读进 `tiles`。图层的矩形不是空的。
fn read_layer_pixels(record: &Record, sample_bytes: usize, canvas: Rect, tiles: &mut TileMap) -> io::Result<Pixels> {
    let rect = record.rect;
    let (w, h) = ((rect.x1 - rect.x0) as usize, (rect.y1 - rect.y0) as usize);
    let mut planes = Vec::with_capacity(4);
    for id in [0, 1, 2, -1] {
        planes.push(match record.channels.iter().find(|c| c.id == id) {
            Some(channel) => match Plane::open(channel.data, w * sample_bytes, h)? {
                Some(plane) => plane,
                None => return Ok(Pixels::Zip),
            },
            // 没有透明度通道的图层整个是不透明的。
            None if id == -1 => Plane::Fill(0xFF),
            None => return Err(invalid(tr("缺少颜色通道", "a color channel is missing"))),
        });
    }
    read_planes(&mut planes, rect, sample_bytes, canvas, tiles)?;
    Ok(Pixels::Read)
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

/// 把矩形 `rect` 的像素读进 `tiles`，出了画布的部分裁掉。`planes` 依次是红、绿、蓝、透明度四个平面，
/// 每个采样 `sample_bytes` 个字节（1 或 2，大端）。四个平面一行一行地同时往下读，不用整幅的缓冲区。
fn read_planes(planes: &mut [Plane], rect: Rect, sample_bytes: usize, canvas: Rect, tiles: &mut TileMap) -> io::Result<()> {
    if rect.intersect(canvas).is_empty() {
        return Ok(());
    }
    let w = (rect.x1 - rect.x0) as usize;
    let row_bytes = w * sample_bytes;
    let mut rows = vec![vec![0u8; row_bytes]; planes.len()];
    let mut pixels = vec![[0u16; 4]; w];
    for y in rect.y0..rect.y1.min(canvas.y1) {
        if y < 0 {
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
        store_row(tiles, canvas.x1, rect.x0 as i64, y, &pixels);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        assert!(back.layers()[0].tiles.get((7, 7)).is_none() && back.layers()[0].tiles.get((-1, 0)).is_none());
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
            // 一个图层组：起点的隐藏记录、两个图层、组本身（隐藏，不透明度 128）。
            Fix::empty("</Layer group>").block(b"lsct", lsct(3)),
            // 左上角伸出画布的图层，带蒙版（−2 号通道和蒙版数据）和图层样式；名字后面跟着一个不认识的块。
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
            // 调整图层和没有像素的填充图层：跳过。
            Fix::empty("Curves 1").block(b"curv", vec![0; 4]),
            Fix::empty("Color Fill 1").block(b"SoCo", vec![0; 4]).block(b"vmsk", vec![0; 8]),
            // 存着像素、但标明了"像素与外观无关"的两个图层：填充图层跳过，另一个读成空图层。
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
        let props: Vec<_> =
            doc.layers().iter().map(|l| (l.props.name.as_str(), l.props.opacity, l.props.visible, l.props.blend)).collect();
        let half = 128.0 / 255.0;
        assert_eq!(
            props,
            [
                ("底色", 1.0, true, BlendMode::Normal),
                // 组是隐藏的、不透明度 128/255，都并进了组里的两个图层。
                ("带蒙版 𝄞", half * half, false, BlendMode::Normal),
                ("Text", half, false, BlendMode::Normal),
                ("frame", 1.0, true, BlendMode::Normal),
                ("顶层", 1.0, true, BlendMode::Screen),
            ]
        );
        let layer = |i: usize| &doc.layers()[i];
        let import = |px: [u8; 4]| to_premultiplied(px.map(|v| v as u16 * 257));
        assert_eq!((pixel(layer(0), 0, 0), pixel(layer(0), 99, 39)), (import([200, 100, 50, 255]), import([200, 100, 50, 255])));
        assert_eq!(content_bounds(&layer(1).tiles, doc.bounds()), Rect { x0: 0, y0: 0, x1: 10, y1: 5 });
        assert_eq!(pixel(layer(1), 9, 4), import([10, 20, 30, 255]));
        assert_eq!(content_bounds(&layer(2).tiles, doc.bounds()), Rect { x0: 90, y0: 10, x1: 100, y1: 12 });
        assert_eq!(pixel(layer(2), 99, 11), import([0, 0, 255, 128]));
        assert!(layer(3).tiles.is_empty());
        assert_eq!(content_bounds(&layer(4).tiles, doc.bounds()), Rect { x0: 0, y0: 38, x1: 2, y1: 40 });

        // 剪贴在下面图层上的那一层，剪贴的开关照原样读进来。
        assert!(layer(2).props.clip && !layer(1).props.clip);

        assert_eq!(notes.len(), 10, "{notes:?}");
        expect_note(&notes, "有 1 个图层组");
        expect_note(&notes, "超出的部分已裁掉：“带蒙版 𝄞”、“Text”");
        expect_note(&notes, "蒙版被忽略了：“带蒙版 𝄞”");
        expect_note(&notes, "图层样式（效果）被忽略了：“带蒙版 𝄞”");
        expect_note(&notes, "文字图层只读入了像素，文字不能再编辑：“Text”");
        expect_note(&notes, "已改为“正常”：“Text（idiv）”");
        expect_note(&notes, "调整图层本程序没有对应的功能，已跳过：“Curves 1”");
        expect_note(&notes, "没有像素，已跳过：“Color Fill 1”、“Gradient Fill 1”");
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
        assert_eq!(names, ["Layer 1", "a", "b"]);
        assert_eq!(
            notes,
            [
                "Flattened the file's layer groups (1) into plain layers (group visibility and opacity were applied to the \
                 layers inside; group blend modes have no effect)",
                r#"Cropped the parts outside the canvas from these layers: "a", "b""#,
                r#"Ignored the masks of these layers: "a""#,
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

        // 图层组的记录不成对（只有组、没有起点；只有起点）也不出事。
        let group = Fix::empty("组").block(b"lsct", lsct(2));
        let end = Fix::empty("</Layer group>").block(b"lsdk", lsct(3));
        assert_eq!(names(&[base.clone(), group.clone(), group.clone()]), ["x"]);
        assert_eq!(names(&[end.clone(), end.clone(), base.clone()]), ["x"]);
        // 嵌套的组：里层隐藏只影响里层的图层，外层的不透明度影响所有的。
        let hidden = group.clone().with(|l| l.flags = 2);
        let faint = group.clone().with(|l| l.opacity = 51);
        let nested = [end.clone(), base.clone(), end.clone(), base.clone(), hidden, base.clone(), faint, base.clone()];
        let (doc, notes) = File::new(4, 4, &nested, [0; 3]).parse().unwrap();
        let props: Vec<_> = doc.layers().iter().map(|l| (l.props.visible, l.props.opacity)).collect();
        assert_eq!(props, [(true, 0.2), (false, 0.2), (true, 0.2), (true, 1.0)]);
        expect_note(&notes, "有 2 个图层组");
        // 整个在画布之外的图层：读进来是空的。
        let outside = [base.clone().with(|l| l.rect = [10, 10, 12, 12]), base.clone().with(|l| l.rect = [-2, 0, 0, 2])];
        let (doc, notes) = File::new(4, 4, &outside, [0; 3]).parse().unwrap();
        assert!(doc.layers().iter().all(|l| l.tiles.is_empty()));
        expect_note(&notes, "超出的部分已裁掉：“x”、“x”");
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

    #[test]
    #[ignore]
    fn interop_read_foreign() {
        interop_load_all(EXTENSION, load);
    }
}
