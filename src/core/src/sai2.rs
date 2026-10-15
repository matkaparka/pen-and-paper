// SPDX-License-Identifier: GPL-3.0-or-later
//! 参照软件第 2 版的文件（`.sai2`）的读写（规格 F-146，实现方案 §3.21、§3.23）。
//!
//! 文件是一张块表加上一串块。图层记录（`layr`）从上往下排，每层的像素在编号相同的 `lpix` 块里，
//! 蒙版在 `mpix` 块里。像素按 32 行高的横条存，一条里是一串 32×32 的块：全透明的一段、一种颜色的一块、
//! 或者压缩过的一块。压缩块里每个通道 1024 个数，存的是"实际值减预测"，用一种变长码写成位流
//!（每个字节从最低位读起）。值是 16 位、预乘透明度，1.0 是 0x4000。
//!
//! 合并图像（`intg`）也读得出来（`merged`），只拿来核对我们合成的结果。
//!
//! 写（`save`）是读的反过来。文件头和 `hist` 里看不出意思的几个数照参照软件自己存的样本填；
//! 这种格式存不下的（填充图层、图层样式等）画进像素或者丢掉，返回给用户的说明里交代。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::document::{BlendMode, Document, Kind, Layer, LayerProps, Mask, Paint, MAX_SIDE};
use crate::lang::tr;
use crate::ora::shared::{self, background_name, invalid, save_atomically, store_row, Notes};
use crate::tile::{GrayMap, TileMap, TILE};
use crate::trf;
use crate::{Rect, ONE};

/// 文件扩展名（不含点）
pub const EXTENSION: &str = "sai2";

const SIGNATURE: &[u8; 16] = b"SAI-CANVAS-TYPE0";
/// 块表里块的个数的上限。正常的文件是几百个，超过的只可能是坏文件。
const MAX_CHUNKS: usize = 1 << 20;
/// 一条图层记录读多少字节就够了（名字和几个参数）。
const MAX_RECORD: u64 = 1 << 16;
/// 一个块（像素数据）读进内存的上限。
const MAX_CHUNK_BYTES: u64 = 1 << 32;
/// 块的边长（像素）。
const BLOCK: usize = 32;
const SAMPLES: usize = BLOCK * BLOCK;
/// 文件里的 1.0。
const FULL: i32 = 0x4000;
/// 文件里没有分辨率，按这个算。
const DEFAULT_DPI: u32 = 72;
/// 写的时候块标记的第一个字节（新文件里都是它）。
const MARK: u8 = 0xFF;
/// 文件头里 0x10 处的标志：0x100 这一位是"白纸"，没有它参照软件把透明的地方画成格子（实现方案 §3.23）；
/// 其余的位意思不知道，照用户 2026 年用参照软件存的文件填。0x1C 处的数也不知道意思，照样本填。
const HEAD_FLAGS: u32 = 0x0100_0100;
const HEAD_1C: u32 = 0x001C_599A;
/// 一串 0 最多写多少个（码里能写到 135，参照软件存的文件里见到的最多是 134）。
const MAX_ZERO_RUN: usize = 134;
/// 图层名最多写多少个 UTF-16 单元。
const MAX_NAME: usize = 255;

struct Chunk {
    tag: [u8; 4],
    id: u32,
    start: u64,
    end: u64,
}

/// 蒙版在图层记录里的那一项（`lmsk`）。
struct MaskRef {
    id: u32,
    /// 范围的左上角和宽高，以 32 像素为单位
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    enabled: bool,
    /// `lmsk` 标志的第一个字节：1 是值表示"挡住多少"，0 是"显示多少"
    kind: u8,
}

struct Record {
    id: u32,
    kind: [u8; 4],
    /// 像素范围（以 32 像素为单位）：左上角、宽、高
    bx: i32,
    by: i32,
    bw: u32,
    bh: u32,
    blend: [u8; 4],
    opacity: u32,
    depth: u16,
    lock_alpha: bool,
    visible: bool,
    clip: bool,
    open: bool,
    name: String,
    mask: Option<MaskRef>,
}

/// 读 `.sai2`：返回文档，以及读入时要告诉用户的话。
pub fn load(path: &Path) -> io::Result<(Document, Vec<String>)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let (w, h, active, chunks) = read_table(&mut file, size)?;
    let mut notes = Notes::default();
    let keep = Document::keep_area(w, h);
    let pixels: HashMap<u32, &Chunk> = chunks.iter().filter(|c| &c.tag == b"lpix").map(|c| (c.id, c)).collect();
    let masks: HashMap<u32, &Chunk> = chunks.iter().filter(|c| &c.tag == b"mpix").map(|c| (c.id, c)).collect();

    // 块表里图层从上往下排，组的记录在它里面的东西前面。
    let mut records = Vec::new();
    for chunk in chunks.iter().filter(|c| &c.tag == b"layr") {
        file.seek(SeekFrom::Start(chunk.start))?;
        let mut rec = Vec::new();
        (&mut file).take((chunk.end - chunk.start).min(MAX_RECORD)).read_to_end(&mut rec)?;
        records.push(parse_record(&rec)?);
    }
    // 本程序的图层列表自下而上，组排在它里面的东西后面：倒过来正好。
    let mut layers = Vec::new();
    let mut ids = Vec::new();
    for r in records.into_iter().rev() {
        let props = LayerProps {
            name: r.name.clone(),
            opacity: r.opacity.min(100) as f32 / 100.0,
            visible: r.visible,
            blend: BlendMode::Normal,
            lock_alpha: r.lock_alpha,
            clip: r.clip,
            ..Default::default()
        };
        let mut layer = match &r.kind {
            b"fold" => {
                let mut folder = Layer::folder(&r.name);
                folder.props = LayerProps { pass_through: &r.blend == b"pass", ..props };
                folder.kind = Kind::Folder { open: r.open };
                folder
            }
            b"norm" => {
                let tiles = match pixels.get(&r.id) {
                    Some(c) => {
                        let data = read_chunk(&mut file, c)?;
                        let mut clipped = false;
                        let tiles = decode_layer(&data, &r, keep, &mut clipped)
                            .map_err(|e| io::Error::new(e.kind(), trf!("图层“{:.80}”的像素：{e}", "Pixels of layer \"{:.80}\": {e}", r.name)))?;
                        if clipped {
                            notes.layer(shared::clipped_kind(), &r.name);
                        }
                        tiles
                    }
                    None => TileMap::default(),
                };
                Layer::pixels(props, tiles)
            }
            _ => {
                notes.layer(tr("下列辅助图层（尺子一类，没有像素）没有读进来", "Skipped these helper layers (rulers and the like, no pixels)"), &r.name);
                continue;
            }
        };
        if &r.kind != b"fold" || &r.blend != b"pass" {
            match blend_mode(&r.blend) {
                Some(mode) => layer.props.blend = mode,
                None => {
                    let code = String::from_utf8_lossy(&r.blend).trim().to_string();
                    notes.layer(shared::unknown_blend_kind(), &trf!("{}（{code}）", "{} ({code})", r.name));
                }
            }
        }
        if let Some(m) = &r.mask {
            if let Some(c) = masks.get(&m.id) {
                let data = read_chunk(&mut file, c)?;
                let mask = decode_mask(&data, m, keep)
                    .map_err(|e| io::Error::new(e.kind(), trf!("图层“{:.80}”的蒙版：{e}", "Mask of layer \"{:.80}\": {e}", r.name)))?;
                layer.mask = Some(mask);
            }
        }
        layer.depth = r.depth;
        layers.push(layer);
        ids.push(r.id);
    }
    let mut doc = Document::from_layers(w, h, [255; 3], layers);
    doc.dpi = DEFAULT_DPI;
    // 文件头里记着参照软件的当前图层（实现方案 §3.21）。
    if let Some(i) = ids.iter().position(|&id| id == active) {
        doc.set_active(i);
    }
    Ok((doc, notes.into_vec()))
}

/// 文件头和块表。返回画布的宽、高、当前图层的编号和各块（各自知道到哪里结束）。
fn read_table(file: &mut File, size: u64) -> io::Result<(i32, i32, u32, Vec<Chunk>)> {
    let mut head = [0u8; 64];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut head).map_err(|_| invalid(tr("文件头不完整", "incomplete header")))?;
    if &head[..16] != SIGNATURE {
        return Err(invalid(tr("不是这种格式的文件（文件头不对）", "not a file of this format (wrong header)")));
    }
    let u32_at = |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
    let (w, h, n) = (u32_at(20), u32_at(24), u32_at(32) as usize);
    if w == 0 || h == 0 || w > MAX_SIDE as u32 || h > MAX_SIDE as u32 {
        return Err(invalid(trf!("画布尺寸 {w}×{h} 不支持（边长最多 {MAX_SIDE}）", "Canvas size {w}×{h} is not supported (at most {MAX_SIDE} on a side)")));
    }
    if n > MAX_CHUNKS || 64 + 16 * n as u64 > size {
        return Err(invalid(tr("块表不对", "bad chunk list")));
    }
    let mut table = vec![0u8; 16 * n];
    file.read_exact(&mut table)?;
    let mut chunks: Vec<Chunk> = table
        .as_chunks::<16>()
        .0
        .iter()
        .map(|e| Chunk {
            tag: [e[0], e[1], e[2], e[3]],
            id: u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            start: u64::from_le_bytes([e[8], e[9], e[10], e[11], e[12], e[13], e[14], e[15]]),
            end: size,
        })
        .collect();
    if chunks.iter().any(|c| c.start < 64 + 16 * n as u64 || c.start > size) {
        return Err(invalid(tr("块表里的位置不对", "bad offsets in the chunk list")));
    }
    // 每块到下一块开始的地方为止；块表本身按图层的顺序排，所以按位置另排一份来找结尾。
    let mut starts: Vec<u64> = chunks.iter().map(|c| c.start).collect();
    starts.sort_unstable();
    starts.dedup();
    for c in &mut chunks {
        let k = starts.partition_point(|&s| s <= c.start);
        c.end = starts.get(k).copied().unwrap_or(size);
    }
    Ok((w as i32, h as i32, u32_at(36), chunks))
}

fn read_chunk(file: &mut File, c: &Chunk) -> io::Result<Vec<u8>> {
    let len = c.end - c.start;
    if len > MAX_CHUNK_BYTES {
        return Err(invalid(tr("数据块太大", "a data chunk is too large")));
    }
    file.seek(SeekFrom::Start(c.start))?;
    let mut data = Vec::new();
    data.try_reserve_exact(len as usize).map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, tr("内存不足", "out of memory")))?;
    file.take(len).read_to_end(&mut data)?;
    Ok(data)
}

/// 图层记录（实现方案 §3.21 的表）。
fn parse_record(rec: &[u8]) -> io::Result<Record> {
    let bad = || invalid(tr("图层记录不完整", "incomplete layer record"));
    if rec.len() < 56 || &rec[..4] != b"layr" {
        return Err(bad());
    }
    let u32_at = |at: usize| u32::from_le_bytes([rec[at], rec[at + 1], rec[at + 2], rec[at + 3]]);
    let i32_at = |at: usize| u32_at(at) as i32;
    let tag = |at: usize| [rec[at], rec[at + 1], rec[at + 2], rec[at + 3]];
    let mut r = Record {
        id: u32_at(4),
        kind: tag(16),
        // 20、24 两个数看着像像素偏移，但按它挪像素和参照软件的合并图像对不上（实现方案 §3.21），不用。
        bx: i32_at(28),
        by: i32_at(32),
        bw: u32_at(36),
        bh: u32_at(40),
        blend: tag(44),
        opacity: u32_at(48),
        depth: rec[52] as u16,
        lock_alpha: rec[53] != 0,
        visible: rec[54] != 0,
        clip: rec[55] & 1 != 0,
        open: rec[55] & 0x40 != 0,
        name: String::new(),
        mask: None,
    };
    // 范围大得离谱的记录当坏文件，免得后面按它分配内存。
    let limit = (4 * MAX_SIDE as i64) / BLOCK as i64;
    if (r.bx as i64).abs() > limit || (r.by as i64).abs() > limit || r.bw as i64 > limit || r.bh as i64 > limit {
        return Err(invalid(tr("图层的范围不对", "bad layer bounds")));
    }
    let mut p = 56;
    while p + 8 <= rec.len() {
        let (name, len) = (tag(p), u32_at(p + 4) as usize);
        if name == [0; 4] {
            break;
        }
        let Some(value) = rec.get(p + 8..p + 8 + len) else { break };
        match &name {
            b"name" if value.len() >= 2 => {
                let count = u16::from_le_bytes([value[0], value[1]]) as usize;
                let units: Vec<u16> = value[2..].as_chunks::<2>().0.iter().take(count).map(|u| u16::from_le_bytes(*u)).collect();
                r.name = String::from_utf16_lossy(&units);
            }
            b"lmsk" if value.len() >= 24 => {
                let v = |k: usize| u32::from_le_bytes([value[k], value[k + 1], value[k + 2], value[k + 3]]);
                r.mask = Some(MaskRef { id: v(0), x: v(4) as i32, y: v(8) as i32, w: v(12), h: v(16), enabled: value[21] != 0, kind: value[20] });
            }
            _ => {}
        }
        p += 8 + len;
    }
    Ok(r)
}

/// 混合模式的编号（实现方案 §3.21），读写共用。参照软件自己特有的五种（`add `、`sub `、`sbad`、`ddge`、`burn`）是本程序的
/// 提亮、压暗、提亮/压暗、柔和减淡、柔和加深（公式是拿用户文件里的合并图像拟合出来的）。组的穿透另写成 `pass`。
const MODES: [(&[u8; 4], BlendMode); 31] = {
    use BlendMode::*;
    [
        (b"norm", Normal),
        (b"dark", Darken),
        (b"mult", Multiply),
        (b"cbrn", ColorBurn),
        (b"burn", SoftBurn),
        (b"lbrn", LinearBurn),
        (b"drkc", DarkerColor),
        (b"litn", Lighten),
        (b"scrn", Screen),
        (b"cddg", ColorDodge),
        (b"ddge", SoftDodge),
        (b"lddg", Add),
        (b"add ", Brighten),
        (b"litc", LighterColor),
        (b"over", Overlay),
        (b"slit", SoftLight),
        (b"hlit", HardLight),
        (b"vlit", VividLight),
        (b"llit", LinearLight),
        (b"sbad", BrightenDim),
        (b"plit", PinLight),
        (b"hmix", HardMix),
        (b"diff", Difference),
        (b"excl", Exclusion),
        (b"fsub", Subtract),
        (b"sub ", Dim),
        (b"fdiv", Divide),
        (b"hue ", Hue),
        (b"sat ", Saturation),
        (b"col ", Color),
        (b"lum ", Luminosity),
    ]
};

fn blend_mode(code: &[u8; 4]) -> Option<BlendMode> {
    MODES.iter().find(|(c, _)| *c == code).map(|&(_, m)| m)
}

fn blend_code(mode: BlendMode) -> &'static [u8; 4] {
    MODES.iter().find(|&&(_, m)| m == mode).map_or(b"norm", |&(c, _)| c)
}

/// 一条一条地走像素数据：`dpcm`，每条的长度，各条的数据。对每一个有内容的块调用 `put(列, 行, 块)`，
/// 列和行以 32 像素为单位，从画布的左上角数。
fn walk_strips(data: &[u8], bx: i32, by: i32, bw: u32, bh: u32, channels: usize, mut put: impl FnMut(i32, i32, &Block)) -> io::Result<()> {
    let bad = |what: &str| invalid(trf!("像素数据不对（{what}）", "bad pixel data ({what})"));
    // 空的图层（范围是 0）连 `dpcm` 都没有。
    if bw == 0 || bh == 0 {
        return Ok(());
    }
    if data.len() < 4 || &data[..4] != b"dpcm" {
        return Err(bad("dpcm"));
    }
    let strips = bh as usize;
    let lens_end = 4 + 4 * strips;
    let lens = data.get(4..lens_end).ok_or_else(|| bad(tr("长度表", "length list")))?;
    let mut at = lens_end;
    for k in 0..strips {
        let len = u32::from_le_bytes([lens[4 * k], lens[4 * k + 1], lens[4 * k + 2], lens[4 * k + 3]]) as usize;
        let s = data.get(at..at + len).ok_or_else(|| bad(tr("长度", "length")))?;
        at += len;
        let row = by + k as i32;
        let (mut p, mut col) = (0usize, bx);
        loop {
            // 标记的第一个字节新文件里是 FF，有些旧文件里是别的值，只看第二个字节。
            let Some(&code) = s.get(p + 1) else { return Err(bad(tr("没有结尾", "no end mark"))) };
            if (code & 15) as i32 != col.rem_euclid(16) {
                return Err(bad(tr("块的标记", "block mark")));
            }
            let u16_at = |q: usize| s.get(q..q + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
            match code >> 4 {
                0x0 => {
                    let n = u16_at(p + 2).ok_or_else(|| bad("run"))?;
                    col += n as i32 + 1;
                    p += 4;
                }
                0x5 => {
                    // 每个通道一个 u16：像素是 B、G、R、A，蒙版只有一个。
                    let c = s.get(p + 2..p + 2 + 2 * channels).ok_or_else(|| bad("solid"))?;
                    let mut v = [0i32; 4];
                    for (slot, b) in v.iter_mut().zip(c.as_chunks::<2>().0) {
                        *slot = u16::from_le_bytes(*b) as i32;
                    }
                    put(col, row, &Block::Solid(v));
                    col += 1;
                    p += 2 + 2 * channels;
                }
                0xA => {
                    let len = u16_at(p + 2).ok_or_else(|| bad("len"))? as usize;
                    let body = s.get(p + 4..p + 4 + len).ok_or_else(|| bad("len"))?;
                    let values = decode_block(body, channels).ok_or_else(|| bad(tr("压缩块", "compressed block")))?;
                    put(col, row, &Block::Coded(values));
                    col += 1;
                    p += 4 + len;
                }
                0xF => break,
                _ => return Err(bad(tr("块的类型", "block type"))),
            }
            if col > bx + bw as i32 {
                return Err(bad(tr("块超出范围", "block outside the bounds")));
            }
        }
    }
    Ok(())
}

/// 一块 32×32：一种颜色（B、G、R、A；蒙版只用第一个），或者逐个像素的值（每个通道 1024 个）。
enum Block {
    Solid([i32; 4]),
    Coded(Vec<[i32; SAMPLES]>),
}

impl Block {
    fn value(&self, channel: usize, k: usize) -> i32 {
        match self {
            Block::Solid(c) => c[channel],
            Block::Coded(v) => v[channel][k],
        }
    }
}

/// 每个字节从最低位读起的位流。
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos >> 3)?;
        let b = (byte >> (self.pos & 7)) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Some(v)
    }
}

/// 读 `count` 个差（实现方案 §3.21 的写法）。
fn residuals(bits: &mut Bits, out: &mut [i32]) -> Option<()> {
    let mut k = 0;
    while k < out.len() {
        let mut zeros = 0;
        while bits.bit()? == 0 {
            zeros += 1;
            if zeros > 7 {
                return None;
            }
        }
        let n = 2 * zeros + 1 + bits.bit()?;
        if n == 16 {
            let run = bits.bits(7)? as usize + 8;
            let end = k.checked_add(run).filter(|&e| e <= out.len())?;
            out[k..end].fill(0);
            k = end;
            continue;
        }
        let magnitude = ((1 << (n - 1)) | bits.bits(n - 1)?) as i32 - 1;
        out[k] = if magnitude != 0 && bits.bit()? == 1 { -magnitude } else { magnitude };
        k += 1;
    }
    Some(())
}

/// 预测：第一行用左边的，第一列用上面的，其余用"左 + 上 − 左上"，夹在 0 到 `max`；值 = 预测 + 差。
fn unpredict(values: &mut [i32], width: usize, max: i32) {
    for k in 0..values.len() {
        let (x, y) = (k % width, k / width);
        let pred = if y == 0 {
            if x == 0 { 0 } else { values[k - 1] }
        } else if x == 0 {
            values[k - width]
        } else {
            (values[k - 1] + values[k - width] - values[k - width - 1]).clamp(0, max)
        };
        values[k] += pred;
    }
}

/// 解一个压缩块：`channels` 个通道，每个 1024 个值。数据不对时返回 None。
fn decode_block(body: &[u8], channels: usize) -> Option<Vec<[i32; SAMPLES]>> {
    let mut bits = Bits { data: body, pos: 0 };
    let mut out = Vec::with_capacity(channels);
    for _ in 0..channels {
        let mut v = [0i32; SAMPLES];
        residuals(&mut bits, &mut v)?;
        unpredict(&mut v, BLOCK, FULL);
        out.push(v);
    }
    Some(out)
}

/// 文件里的值（1.0 = 0x4000）换成本程序的 15 位。
#[inline]
fn to15(v: i32) -> u16 {
    (v.clamp(0, FULL) as u32 * (ONE / FULL as u32)) as u16
}

/// 一个图层的像素。伸出 `keep` 的部分裁掉，裁掉了看得见的像素时把 `clipped` 置上。
fn decode_layer(data: &[u8], r: &Record, keep: Rect, clipped: &mut bool) -> io::Result<TileMap> {
    let mut tiles = TileMap::default();
    walk_strips(data, r.bx, r.by, r.bw, r.bh, 4, |col, row, block| {
        let (x0, y0) = (col * BLOCK as i32, row * BLOCK as i32);
        if let Block::Solid([_, _, _, 0]) = block {
            return;
        }
        for y in 0..BLOCK {
            let py = y0 + y as i32;
            for x in 0..BLOCK {
                let k = y * BLOCK + x;
                let a = block.value(3, k);
                if a <= 0 {
                    continue;
                }
                let px = x0 + x as i32;
                if px < keep.x0 || px >= keep.x1 || py < keep.y0 || py >= keep.y1 {
                    *clipped = true;
                    continue;
                }
                // 文件里是 B、G、R、A，本程序的分块是 R、G、B、A；颜色不超过透明度。
                let a15 = to15(a);
                let c = |ch: usize| to15(block.value(ch, k)).min(a15);
                let t = tiles.get_mut((px.div_euclid(TILE), py.div_euclid(TILE)));
                let i = ((py.rem_euclid(TILE) * TILE + px.rem_euclid(TILE)) * 4) as usize;
                t.0[i..i + 4].copy_from_slice(&[c(2), c(1), c(0), a15]);
            }
        }
    })?;
    Ok(tiles)
}

/// 一个蒙版（实现方案 §3.21）。`lmsk` 标志的第一个字节是 1 时文件里的值是"挡住多少"，范围以外全显示；
/// 是 0 时值是"显示多少"，范围以外全挡住。本程序的蒙版是"显示多少"。
fn decode_mask(data: &[u8], m: &MaskRef, keep: Rect) -> io::Result<Mask> {
    let hides = m.kind != 0;
    let default = if hides { ONE as u16 } else { 0 };
    let mut tiles = GrayMap::default();
    walk_strips(data, m.x, m.y, m.w, m.h, 1, |col, row, block| {
        let (x0, y0) = (col * BLOCK as i32, row * BLOCK as i32);
        for y in 0..BLOCK {
            let py = y0 + y as i32;
            for x in 0..BLOCK {
                let v = block.value(0, y * BLOCK + x);
                let px = x0 + x as i32;
                // 值是 0 的地方和范围以外一样，不用写。
                if v <= 0 || px < keep.x0 || px >= keep.x1 || py < keep.y0 || py >= keep.y1 {
                    continue;
                }
                let t = tiles.get_mut((px.div_euclid(TILE), py.div_euclid(TILE)), default);
                t[(py.rem_euclid(TILE) * TILE + px.rem_euclid(TILE)) as usize] = if hides { ONE as u16 - to15(v) } else { to15(v) };
            }
        }
    })?;
    Ok(Mask { tiles, default, disabled: !m.enabled })
}

/// 文件里存的合并图像：8 位、不预乘的 RGBA，逐行；旧文件里没有透明度（返回的第三项是 false，透明度一律 255）。
/// 只拿来和我们合成的结果对照。
pub fn merged(path: &Path) -> io::Result<(i32, i32, Vec<u8>, bool)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let (w, h, _, chunks) = read_table(&mut file, size)?;
    let chunk = chunks.iter().find(|c| &c.tag == b"intg").ok_or_else(|| invalid(tr("没有合并图像", "no merged image")))?;
    let data = read_chunk(&mut file, chunk)?;
    let bad = || invalid(tr("合并图像的数据不对", "bad merged image data"));
    let (w, h) = (w as usize, h as usize);
    let (tx, ty) = (w.div_ceil(256), h.div_ceil(256));
    if data.len() < 4 + 4 * tx * ty || &data[..4] != b"dpcm" {
        return Err(bad());
    }
    // 一个格子：两字节的标记，然后逐行：各通道（B、G、R，新文件还有 A）一串，每行结束对齐到字节。
    let decode = |s: &[u8], tw: usize, th: usize, channels: usize| -> Option<Vec<Vec<i32>>> {
        let mut bits = Bits { data: s.get(2..)?, pos: 0 };
        let mut planes = vec![vec![0i32; tw * th]; channels];
        for y in 0..th {
            for plane in planes.iter_mut() {
                residuals(&mut bits, &mut plane[y * tw..(y + 1) * tw])?;
            }
            bits.pos = bits.pos.div_ceil(8) * 8;
        }
        // 正好用完才算对（新旧两种文件就靠这个分）。
        (bits.pos == (s.len() - 2) * 8).then_some(planes)
    };
    let mut rgba = vec![0u8; w * h * 4];
    let mut channels = 0;
    let mut at = 4 + 4 * tx * ty;
    for gy in 0..ty {
        for gx in 0..tx {
            let t = gy * tx + gx;
            let len = u32::from_le_bytes(data[4 + 4 * t..8 + 4 * t].try_into().unwrap()) as usize;
            let s = data.get(at..at + len).ok_or_else(bad)?;
            at += len;
            let (tw, th) = ((w - gx * 256).min(256), (h - gy * 256).min(256));
            let planes = match channels {
                0 => {
                    let found = decode(s, tw, th, 4).map(|p| (4, p)).or_else(|| decode(s, tw, th, 3).map(|p| (3, p))).ok_or_else(bad)?;
                    channels = found.0;
                    found.1
                }
                n => decode(s, tw, th, n).ok_or_else(bad)?,
            };
            let mut planes = planes;
            for plane in planes.iter_mut() {
                unpredict(plane, tw, 255);
            }
            for y in 0..th {
                for x in 0..tw {
                    let o = ((gy * 256 + y) * w + gx * 256 + x) * 4;
                    let k = y * tw + x;
                    let a = if channels == 4 { planes[3][k] } else { 255 };
                    rgba[o..o + 4].copy_from_slice(&[planes[2][k], planes[1][k], planes[0][k], a].map(|v| v.clamp(0, 255) as u8));
                }
            }
        }
        // 每一行格子后面还有两字节的结束标记。
        at += 2;
    }
    Ok((w as i32, h as i32, rgba, channels == 4))
}

// ---- 写（实现方案 §3.23） ----

/// 存成 `.sai2`。返回要告诉用户的话：这种格式存不下、画进了像素或者丢掉了的东西（实现方案 §3.23 的表）。
///
/// 和 `format::save` 一样，先写进同一目录下的临时文件，完整落盘之后再改名顶替 `path`。
/// 不会把文档标成"已保存"，那是调用方的事。
pub fn save(doc: &Document, path: &Path) -> io::Result<Vec<String>> {
    shared::check_document(doc)?;
    let (layers, active, notes) = prepare(doc);
    save_atomically(path, |file| write_file(doc, &layers, active, file))?;
    Ok(notes)
}

/// 要写进去的图层（自下而上）、当前图层是其中第几个，以及说明。填充图层画成像素，样式、画板这些去掉；
/// 背景色不是白的时最底下加一层"背景"（参照软件没有背景色，图层底下是白纸）。
fn prepare(doc: &Document) -> (Vec<Layer>, usize, Vec<String>) {
    let mut notes = Notes::default();
    let mut out = Vec::with_capacity(doc.layers().len() + 1);
    if doc.background != [255; 3] {
        let [r, g, b] = doc.background.map(|c| c as u16 * 257);
        let row = vec![[r, g, b, u16::MAX]; doc.width as usize];
        let mut tiles = TileMap::default();
        for y in 0..doc.height {
            store_row(&mut tiles, doc.bounds(), 0, y, &row);
        }
        out.push(Layer::pixels(LayerProps { name: background_name().into(), ..Default::default() }, tiles));
    }
    let below = out.len();
    for (i, src) in doc.layers().iter().enumerate() {
        let name = src.props.name.as_str();
        let mut layer = Layer { style: None, artboard: None, fx: Default::default(), ..src.clone() };
        if let Kind::Fill(fill) = &src.kind {
            notes.layer(
                if fill.paint == Paint::Other {
                    tr("下列填充图层（渐变、图案）本程序画不出来，存成了空图层", "These fill layers (gradients, patterns) cannot be drawn here and were saved empty")
                } else {
                    tr("下列填充图层存成了普通图层（这种格式没有填充图层）", "These fill layers were saved as pixel layers (the format has no fill layers)")
                },
                name,
            );
            layer.kind = Kind::Pixels;
            layer.tiles = doc.drawn_pixels(i);
        }
        if src.style.is_some() {
            notes.layer(tr("下列图层的图层样式（投影、发光等）没有存进去", "Layer styles (shadows, glows…) of these layers were not saved"), name);
        }
        if src.artboard.is_some() {
            notes.layer(tr("下列画板存成了普通的组", "These artboards were saved as plain groups"), name);
        }
        if src.props.fill != 1.0 {
            notes.layer(tr("下列图层的填充不透明度并进了不透明度", "The Fill opacity of these layers was merged into their opacity"), name);
            layer.props.opacity *= src.props.fill.clamp(0.0, 1.0);
            layer.props.fill = 1.0;
        }
        if !src.props.shapes {
            notes.layer(tr("下列图层的“透明形状图层”设置没有存进去", "The Transparency Shapes setting of these layers was not saved"), name);
            layer.props.shapes = true;
        }
        out.push(layer);
    }
    (out, doc.active() + below, notes.into_vec())
}

/// 像素或蒙版的范围，以 32 像素为单位。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Area {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Area {
    /// 盖住 `r` 的那些块；`r` 是空的就是空的范围。
    fn covering(r: Rect) -> Area {
        if r.is_empty() {
            return Area::default();
        }
        let b = BLOCK as i32;
        let (x0, y0, x1, y1) = (r.x0.div_euclid(b), r.y0.div_euclid(b), (r.x1 + b - 1).div_euclid(b), (r.y1 + b - 1).div_euclid(b));
        Area { x: x0, y: y0, w: (x1 - x0) as u32, h: (y1 - y0) as u32 }
    }

    fn bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&self.x.to_le_bytes());
        out[4..8].copy_from_slice(&self.y.to_le_bytes());
        out[8..12].copy_from_slice(&self.w.to_le_bytes());
        out[12..16].copy_from_slice(&self.h.to_le_bytes());
        out
    }
}

/// 一个图层要写的东西：编号、像素的范围、蒙版（编号、范围）。
struct Plan {
    id: u32,
    area: Area,
    mask: Option<(u32, Area)>,
}

fn write_file(doc: &Document, layers: &[Layer], active: usize, file: File) -> io::Result<()> {
    let keep = doc.keep_bounds();
    // 编号：图层自下而上从 2 起，蒙版接在后面，也是自下而上（参照软件存的样本是这样）。
    let mut next_mask = 2 + layers.len() as u32;
    let plans: Vec<Plan> = layers
        .iter()
        .enumerate()
        .map(|(i, l)| Plan {
            id: 2 + i as u32,
            area: if l.is_folder() { Area::default() } else { layer_area(&l.tiles, keep) },
            mask: l.mask.as_ref().map(|m| {
                next_mask += 1;
                // 整个蒙版都和范围外一样时写一块空的。
                (next_mask - 1, mask_area(m, keep).unwrap_or(Area { x: 0, y: 0, w: 1, h: 1 }))
            }),
        })
        .collect();

    // 块表：hist、intg、图层记录（从上往下）、各层的像素（从上往下，每层先 lpix 后 mpix）。
    let mut table: Vec<([u8; 4], u32)> = vec![(*b"hist", 0), (*b"intg", 0)];
    table.extend(plans.iter().rev().map(|p| (*b"layr", p.id)));
    for (l, p) in layers.iter().zip(&plans).rev() {
        if !l.is_folder() {
            table.push((*b"lpix", p.id));
        }
        if let Some((id, _)) = p.mask {
            table.push((*b"mpix", id));
        }
    }

    let mut out = BufWriter::new(file);
    let mut head = Vec::with_capacity(64);
    head.extend(SIGNATURE);
    for v in [HEAD_FLAGS, doc.width as u32, doc.height as u32, HEAD_1C, table.len() as u32, plans[active].id] {
        head.extend(v.to_le_bytes());
    }
    head.extend([0u8; 16]);
    head.extend([0x80, 0x80, 0x80, 0xFF]);
    head.extend(b"norm");
    out.write_all(&head)?;
    out.write_all(&vec![0u8; 16 * table.len()])?;

    let mut at = (head.len() + 16 * table.len()) as u64;
    let mut starts = Vec::with_capacity(table.len());
    let mut put = |out: &mut BufWriter<File>, data: &[u8]| -> io::Result<()> {
        starts.push(at);
        at += data.len() as u64;
        out.write_all(data)
    };
    put(&mut out, &hist())?;
    // 合并图像拿要写进去的图层合成，底下不垫背景色（参照软件的合并图像有透明度）。
    let merged = Document::from_layers(doc.width, doc.height, doc.background, layers.to_vec()).flatten_rgba();
    put(&mut out, &encode_merged(doc.width as usize, doc.height as usize, &merged))?;
    drop(merged);
    for (l, p) in layers.iter().zip(&plans).rev() {
        put(&mut out, &record(l, p))?;
    }
    for (l, p) in layers.iter().zip(&plans).rev() {
        if !l.is_folder() {
            put(&mut out, &encode_layer(&l.tiles, keep, p.area))?;
        }
        if let (Some(m), Some((_, area))) = (&l.mask, p.mask) {
            put(&mut out, &encode_mask(m, keep, area))?;
        }
    }

    out.seek(SeekFrom::Start(head.len() as u64))?;
    for ((tag, id), start) in table.iter().zip(&starts) {
        out.write_all(tag)?;
        out.write_all(&id.to_le_bytes())?;
        out.write_all(&start.to_le_bytes())?;
    }
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

/// `hist`：保存时间（协调世界时）"年月日.时分秒.2"补 0 到 64 字节，16 个随机字节，u64 的 1（实现方案 §3.23）。
fn hist() -> Vec<u8> {
    use std::hash::BuildHasher;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let t = secs % 86_400;
    let text = format!("{year:04}{month:02}{day:02}.{:02}{:02}{:02}.2", t / 3600, t / 60 % 60, t % 60);
    let mut out: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    out.resize(64, 0);
    // 标准库的散列函数每次用随机的键，拿它当随机数就够了。
    let random = std::collections::hash_map::RandomState::new();
    for k in 0..2u8 {
        out.extend(random.hash_one((now.as_nanos(), k)).to_le_bytes());
    }
    out.extend(1u64.to_le_bytes());
    out
}

/// 从 1970-01-01 起的第几天换成公历的年、月、日。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (month <= 2) as i64, month, day)
}

/// 一条图层记录（实现方案 §3.21 的表）。8、12、20、24 写 0。
fn record(layer: &Layer, plan: &Plan) -> Vec<u8> {
    let p = &layer.props;
    let folder = layer.is_folder();
    let mut r = Vec::with_capacity(128);
    r.extend(b"layr");
    r.extend(plan.id.to_le_bytes());
    r.extend([0u8; 8]);
    r.extend(if folder { b"fold" } else { b"norm" });
    r.extend([0u8; 8]);
    r.extend(plan.area.bytes());
    r.extend(if folder && p.pass_through { b"pass" } else { blend_code(p.blend) });
    let opacity = if p.opacity.is_nan() { 0 } else { (p.opacity.clamp(0.0, 1.0) * 100.0).round() as u32 };
    r.extend(opacity.to_le_bytes());
    let open = matches!(layer.kind, Kind::Folder { open: true });
    r.extend([layer.depth.min(255) as u8, p.lock_alpha as u8, p.visible as u8, p.clip as u8 | if open { 0x40 } else { 0 }]);
    // 名字：u16 字数加 UTF-16，补 0 到 4 字节的倍数。截断时不把一对代理拆开。
    let mut units: Vec<u16> = p.name.encode_utf16().collect();
    if units.len() > MAX_NAME {
        let cut = if (0xD800..0xDC00).contains(&units[MAX_NAME - 1]) { MAX_NAME - 1 } else { MAX_NAME };
        units.truncate(cut);
    }
    let mut name = (units.len() as u16).to_le_bytes().to_vec();
    name.extend(units.iter().flat_map(|u| u.to_le_bytes()));
    pad4(&mut name);
    param(&mut r, b"name", &name);
    if let (Some(m), Some((id, area))) = (&layer.mask, plan.mask) {
        // 默认白的写成"挡住多少"（标志 1），默认黑的写成"显示多少"（标志 0）；第二个字节是启用。
        let mut v = id.to_le_bytes().to_vec();
        v.extend(area.bytes());
        v.extend([hides(m) as u8, !m.disabled as u8, 1, 0]);
        param(&mut r, b"lmsk", &v);
    }
    r.extend([0u8; 4]);
    r
}

fn param(r: &mut Vec<u8>, name: &[u8; 4], value: &[u8]) {
    r.extend(name);
    r.extend((value.len() as u32).to_le_bytes());
    r.extend(value);
}

fn pad4(v: &mut Vec<u8>) {
    v.resize(v.len().next_multiple_of(4), 0);
}

/// 蒙版是不是写成"挡住多少"的那种（默认白的）。
fn hides(m: &Mask) -> bool {
    m.default != 0
}

/// 本程序的 15 位换成文件的 16 位（1.0 = 0x4000），四舍五入。
#[inline]
fn to16(v: u16) -> i32 {
    ((v as i32).min(ONE as i32) + 1) >> 1
}

/// 图层的一块 32×32 换成文件里的 B、G、R、A。`keep` 以外的像素当透明。返回这块是不是全透明。
fn layer_block(tiles: &TileMap, keep: Rect, col: i32, row: i32, out: &mut [[i32; SAMPLES]]) -> bool {
    let (x0, y0) = (col * BLOCK as i32, row * BLOCK as i32);
    let Some(tile) = tiles.get((x0.div_euclid(TILE), y0.div_euclid(TILE))) else { return true };
    let (ox, oy) = (x0.rem_euclid(TILE) as usize, y0.rem_euclid(TILE) as usize);
    let mut empty = true;
    for y in 0..BLOCK {
        for x in 0..BLOCK {
            let k = y * BLOCK + x;
            let (px, py) = (x0 + x as i32, y0 + y as i32);
            let i = ((oy + y) * TILE as usize + ox + x) * 4;
            let a = if px < keep.x0 || px >= keep.x1 || py < keep.y0 || py >= keep.y1 { 0 } else { to16(tile.0[i + 3]) };
            // 分块里是 R、G、B、A，文件里是 B、G、R、A；颜色不超过透明度。
            for (c, ch) in [2, 1, 0].into_iter().enumerate() {
                out[c][k] = to16(tile.0[i + ch]).min(a);
            }
            out[3][k] = a;
            empty &= a == 0;
        }
    }
    empty
}

/// 蒙版的一块换成文件里的值（默认白的是"挡住多少"）。`keep` 以外当范围外。返回这块是不是全是 0（和范围外一样）。
fn mask_block(mask: &Mask, keep: Rect, col: i32, row: i32, out: &mut [[i32; SAMPLES]]) -> bool {
    let (x0, y0) = (col * BLOCK as i32, row * BLOCK as i32);
    let tile = mask.tile((x0.div_euclid(TILE), y0.div_euclid(TILE)));
    let (ox, oy) = (x0.rem_euclid(TILE) as usize, y0.rem_euclid(TILE) as usize);
    let hides = hides(mask);
    let mut empty = true;
    for y in 0..BLOCK {
        for x in 0..BLOCK {
            let (px, py) = (x0 + x as i32, y0 + y as i32);
            let inside = px >= keep.x0 && px < keep.x1 && py >= keep.y0 && py < keep.y1;
            let v = match tile {
                Some(t) if inside => t[(oy + y) * TILE as usize + ox + x],
                _ => mask.default,
            };
            let f = to16(if hides { ONE as u16 - v.min(ONE as u16) } else { v });
            out[0][y * BLOCK + x] = f;
            empty &= f == 0;
        }
    }
    empty
}

/// 有内容的块的范围。`coords` 是有分块的地方，`empty(列, 行, 草稿)` 说一块是不是空的。按文件里的精度看，
/// 透明度低到 8 位下看不见的像素也算内容（所以不用 `content_bounds`）。
fn area_of(coords: impl Iterator<Item = (i32, i32)>, keep: Rect, channels: usize, mut empty: impl FnMut(i32, i32, &mut [[i32; SAMPLES]]) -> bool) -> Option<Area> {
    let mut scratch = vec![[0i32; SAMPLES]; channels];
    let mut r = Rect::EMPTY;
    let per_tile = TILE / BLOCK as i32;
    for (tx, ty) in coords {
        for sub in 0..per_tile * per_tile {
            let (col, row) = (tx * per_tile + sub % per_tile, ty * per_tile + sub / per_tile);
            let b = Rect { x0: col * BLOCK as i32, y0: row * BLOCK as i32, x1: (col + 1) * BLOCK as i32, y1: (row + 1) * BLOCK as i32 };
            if !b.intersect(keep).is_empty() && r.union(b) != r && !empty(col, row, &mut scratch) {
                r = r.union(b);
            }
        }
    }
    (!r.is_empty()).then(|| Area::covering(r))
}

/// 图层像素的范围；完全空的图层范围是 0。
fn layer_area(tiles: &TileMap, keep: Rect) -> Area {
    area_of(tiles.coords(), keep, 4, |col, row, out| layer_block(tiles, keep, col, row, out)).unwrap_or_default()
}

/// 蒙版里和范围外不一样的那些块的范围；整个都一样时是 None。
fn mask_area(mask: &Mask, keep: Rect) -> Option<Area> {
    area_of(mask.tiles.coords(), keep, 1, |col, row, out| mask_block(mask, keep, col, row, out))
}

/// 图层的像素块（`lpix`）。范围是空的就是 4 个字节的 0。
fn encode_layer(tiles: &TileMap, keep: Rect, area: Area) -> Vec<u8> {
    if area.w == 0 || area.h == 0 {
        return vec![0; 4];
    }
    encode_strips(area, 4, |col, row, out| layer_block(tiles, keep, col, row, out))
}

fn encode_mask(mask: &Mask, keep: Rect, area: Area) -> Vec<u8> {
    encode_strips(area, 1, |col, row, out| mask_block(mask, keep, col, row, out))
}

/// `dpcm`、每条的长度、各条（实现方案 §3.23）。`block(列, 行, 值)` 填好一块的值，返回这块是不是全是 0。
/// 每条从范围左边写到右边：全是 0 的连成一段写类型 0，一种颜色的写类型 5，其余写类型 A，最后类型 F。
fn encode_strips(area: Area, channels: usize, mut block: impl FnMut(i32, i32, &mut [[i32; SAMPLES]]) -> bool) -> Vec<u8> {
    let mut values = vec![[0i32; SAMPLES]; channels];
    let end = area.x + area.w as i32;
    let mut strips = Vec::with_capacity(area.h as usize);
    for row in area.y..area.y + area.h as i32 {
        let mut s = Vec::new();
        let mut run = 0i32;
        let flush = |s: &mut Vec<u8>, run: &mut i32, col: i32| {
            if *run > 0 {
                s.extend([MARK, (col - *run).rem_euclid(16) as u8]);
                s.extend((*run as u16 - 1).to_le_bytes());
                *run = 0;
            }
        };
        for col in area.x..end {
            if block(col, row, &mut values) {
                run += 1;
                continue;
            }
            flush(&mut s, &mut run, col);
            let low = col.rem_euclid(16) as u8;
            if values.iter().all(|v| v.iter().all(|&x| x == v[0])) {
                s.extend([MARK, 0x50 | low]);
                s.extend(values.iter().flat_map(|v| (v[0] as u16).to_le_bytes()));
            } else {
                let mut bits = BitWriter::default();
                for v in &values {
                    put_residuals(&mut bits, &residuals_of(v, BLOCK, FULL));
                }
                let body = bits.finish();
                s.extend([MARK, 0xA0 | low]);
                s.extend((body.len() as u16).to_le_bytes());
                s.extend(body);
            }
        }
        flush(&mut s, &mut run, end);
        s.extend([MARK, 0xF0 | end.rem_euclid(16) as u8]);
        strips.push(s);
    }
    dpcm(&strips)
}

fn dpcm(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"dpcm".to_vec();
    parts.iter().for_each(|p| out.extend((p.len() as u32).to_le_bytes()));
    parts.iter().for_each(|p| out.extend(p));
    pad4(&mut out);
    out
}

/// 合并图像（`intg`）：`rgba` 是整张画布 8 位、不预乘的 RGBA。256×256 一格，格子里逐行 B、G、R、A 各一串，
/// 每行补齐到字节；每一行格子后面是 `FF` 加格子的列数。
fn encode_merged(w: usize, h: usize, rgba: &[u8]) -> Vec<u8> {
    let (tx, ty) = (w.div_ceil(256), h.div_ceil(256));
    let mut cells = Vec::with_capacity(tx * ty);
    for gy in 0..ty {
        for gx in 0..tx {
            let (tw, th) = ((w - gx * 256).min(256), (h - gy * 256).min(256));
            let mut planes = vec![vec![0i32; tw * th]; 4];
            for y in 0..th {
                for x in 0..tw {
                    let o = ((gy * 256 + y) * w + gx * 256 + x) * 4;
                    for (c, ch) in [2, 1, 0, 3].into_iter().enumerate() {
                        planes[c][y * tw + x] = rgba[o + ch] as i32;
                    }
                }
            }
            let res: Vec<Vec<i32>> = planes.iter().map(|p| residuals_of(p, tw, 255)).collect();
            let mut bits = BitWriter::default();
            for y in 0..th {
                for r in &res {
                    put_residuals(&mut bits, &r[y * tw..(y + 1) * tw]);
                }
                bits.align();
            }
            let mut cell = vec![MARK, gx as u8];
            cell.extend(bits.finish());
            cells.push(cell);
        }
    }
    let mut out = b"dpcm".to_vec();
    cells.iter().for_each(|c| out.extend((c.len() as u32).to_le_bytes()));
    for row in cells.chunks(tx) {
        row.iter().for_each(|c| out.extend(c));
        out.extend([MARK, tx as u8]);
    }
    pad4(&mut out);
    out
}

/// 每个字节从最低位写起的位流（`Bits` 反过来）。
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    /// 写 `v` 的低 `n` 位（n ≤ 32），从最低位起。
    fn put(&mut self, v: u32, n: u32) {
        self.acc |= (v as u64 & ((1u64 << n) - 1)) << self.n;
        self.n += n;
        while self.n >= 8 {
            self.bytes.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// 补齐到字节。
    fn align(&mut self) {
        if self.n > 0 {
            self.bytes.push(self.acc as u8);
            self.acc = 0;
            self.n = 0;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.bytes
    }
}

/// 值换成差（预测同 `unpredict`）。
fn residuals_of(values: &[i32], width: usize, max: i32) -> Vec<i32> {
    (0..values.len())
        .map(|k| {
            let (x, y) = (k % width, k / width);
            let pred = if y == 0 {
                if x == 0 { 0 } else { values[k - 1] }
            } else if x == 0 {
                values[k - width]
            } else {
                (values[k - 1] + values[k - width] - values[k - width - 1]).clamp(0, max)
            };
            values[k] - pred
        })
        .collect()
}

/// 差的写法（`residuals` 反过来）：0 连着 8 个以上写成一串，其余逐个写。
fn put_residuals(bits: &mut BitWriter, res: &[i32]) {
    let mut k = 0;
    while k < res.len() {
        let r = res[k];
        if r == 0 {
            let run = res[k..].iter().take(MAX_ZERO_RUN).take_while(|&&v| v == 0).count();
            if run >= 8 {
                // 7 个 0、1、1（n = 16），再 7 位：个数减 8。
                bits.put(0b11 << 7, 9);
                bits.put((run - 8) as u32, 7);
                k += run;
            } else {
                // n = 1：1、0。
                bits.put(0b01, 2);
                k += 1;
            }
            continue;
        }
        // |差| + 1 有 n 位：z = (n − 1) / 2 个 0、1、f = (n − 1) % 2，再 n − 1 位，最后符号。
        let m = r.unsigned_abs() + 1;
        let n = 32 - m.leading_zeros();
        let z = (n - 1) / 2;
        bits.put(1 << z | ((n - 1) % 2) << (z + 1), z + 2);
        bits.put(m - (1 << (n - 1)), n - 1);
        bits.put((r < 0) as u32, 1);
        k += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 一块压缩块：各通道 1024 个值。
    fn coded_block(channels: &[Vec<i32>]) -> Vec<u8> {
        let mut w = BitWriter::default();
        for c in channels {
            put_residuals(&mut w, &residuals_of(c, BLOCK, FULL));
        }
        w.finish()
    }

    /// 一条里的一段。
    enum Piece {
        Run(u16),
        Solid(Vec<u16>),
        Coded(Vec<u8>),
    }

    /// 一条：从第 `first_col` 列起依次排好，`marker` 是标记的第一个字节。
    fn strip(marker: u8, first_col: i32, pieces: &[Piece]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut col = first_col;
        for p in pieces {
            let low = col.rem_euclid(16) as u8;
            match p {
                Piece::Run(n) => {
                    out.extend([marker, low]);
                    out.extend(n.to_le_bytes());
                    col += *n as i32 + 1;
                }
                Piece::Solid(v) => {
                    out.extend([marker, 0x50 | low]);
                    v.iter().for_each(|x| out.extend(x.to_le_bytes()));
                    col += 1;
                }
                Piece::Coded(d) => {
                    out.extend([marker, 0xA0 | low]);
                    out.extend((d.len() as u16).to_le_bytes());
                    out.extend(d);
                    col += 1;
                }
            }
        }
        out.extend([marker, 0xF0 | col.rem_euclid(16) as u8]);
        out
    }

    /// 一条图层记录。`area` 是（左, 上, 宽, 高），以 32 像素为单位。
    #[allow(clippy::too_many_arguments)]
    fn raw_record(id: u32, kind: &[u8; 4], area: (i32, i32, u32, u32), blend: &[u8; 4], opacity: u32, flags: [u8; 4], name: &str, mask: Option<[u8; 24]>) -> Vec<u8> {
        let mut r = b"layr".to_vec();
        r.extend(id.to_le_bytes());
        r.extend([0u8; 8]);
        r.extend(kind);
        r.extend(0i32.to_le_bytes());
        r.extend(0i32.to_le_bytes());
        r.extend(area.0.to_le_bytes());
        r.extend(area.1.to_le_bytes());
        r.extend(area.2.to_le_bytes());
        r.extend(area.3.to_le_bytes());
        r.extend(blend);
        r.extend(opacity.to_le_bytes());
        r.extend(flags);
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut value = (units.len() as u16).to_le_bytes().to_vec();
        units.iter().for_each(|u| value.extend(u.to_le_bytes()));
        r.extend(b"name");
        r.extend((value.len() as u32).to_le_bytes());
        r.extend(value);
        if let Some(m) = mask {
            r.extend(b"lmsk");
            r.extend(24u32.to_le_bytes());
            r.extend(m);
        }
        r.extend([0u8; 4]);
        r
    }

    /// 整个文件：块表按给的顺序（图层从上往下）。
    fn file(w: u32, h: u32, chunks: &[(&[u8; 4], u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        out.extend(0x100u32.to_le_bytes());
        out.extend(w.to_le_bytes());
        out.extend(h.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend((chunks.len() as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend([0u8; 16]);
        out.extend([0x80, 0x80, 0x80, 0xff]);
        out.extend(b"norm");
        let mut at = 64 + 16 * chunks.len() as u64;
        for (tag, id, data) in chunks {
            out.extend(*tag);
            out.extend(id.to_le_bytes());
            out.extend(at.to_le_bytes());
            at += data.len() as u64;
        }
        for (_, _, data) in chunks {
            out.extend(data);
        }
        out
    }

    fn pixel(tiles: &TileMap, x: i32, y: i32) -> [u16; 4] {
        let i = ((y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) * 4) as usize;
        tiles.get((x.div_euclid(TILE), y.div_euclid(TILE))).map_or([0; 4], |t| [t.0[i], t.0[i + 1], t.0[i + 2], t.0[i + 3]])
    }

    /// 蒙版标志：编号 `id`，范围 (x, y, 1, 1)，第一个字节 `kind`。
    fn lmsk(id: u32, x: i32, kind: u8) -> [u8; 24] {
        let mut m = [0u8; 24];
        m[0..4].copy_from_slice(&id.to_le_bytes());
        m[4..8].copy_from_slice(&x.to_le_bytes());
        m[12..16].copy_from_slice(&1u32.to_le_bytes());
        m[16..20].copy_from_slice(&1u32.to_le_bytes());
        m[20..24].copy_from_slice(&[kind, 1, 1, 0]);
        m
    }

    fn load_bytes(bytes: &[u8]) -> io::Result<(Document, Vec<String>)> {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zhibi-sai2-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sai2");
        std::fs::write(&path, bytes).unwrap();
        let result = load(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        result
    }

    #[test]
    fn reads_layers_groups_masks_and_all_block_kinds() {
        // 第一块：B、G、R 随位置变（预乘：不超过透明度），A 是 0x4000。
        let ramp = |c: i32| (0..SAMPLES as i32).map(|k| ((k % 32) * 300 + (k / 32) * 100 + c * 50).min(FULL)).collect::<Vec<i32>>();
        let coded = coded_block(&[ramp(0), ramp(1), ramp(2), vec![FULL; SAMPLES]]);
        // 蒙版块：左半是 0x4000、右半是 0。
        let half: Vec<i32> = (0..SAMPLES).map(|k| if k % 32 < 16 { FULL } else { 0 }).collect();
        let pixels = dpcm(&[
            strip(0xff, 0, &[Piece::Coded(coded), Piece::Solid(vec![0x1000, 0x2000, 0x3000, 0x4000]), Piece::Run(0)]),
            // 有些旧文件的标记不是 FF。
            strip(0x8d, 0, &[Piece::Run(2)]),
        ]);
        let white = || dpcm(&[strip(0xff, 1, &[Piece::Solid(vec![0x4000, 0x4000, 0x4000, 0x4000])])]);
        let mask = || dpcm(&[strip(0xff, 1, &[Piece::Coded(coded_block(std::slice::from_ref(&half)))])]);
        let bytes = file(
            100,
            70,
            &[
                (b"layr", 4, raw_record(4, b"fold", (0, 0, 0, 0), b"pass", 100, [0, 0, 1, 0x40], "组", None)),
                (b"layr", 6, raw_record(6, b"norm", (1, 0, 1, 1), b"norm", 100, [1, 0, 1, 0], "显示", Some(lmsk(10, 1, 0)))),
                (b"layr", 3, raw_record(3, b"norm", (1, 0, 1, 1), b"mult", 50, [1, 0, 1, 0], "挡住", Some(lmsk(9, 1, 1)))),
                (b"layr", 2, raw_record(2, b"norm", (0, 0, 3, 2), b"add ", 100, [0, 1, 1, 1], "Layer", None)),
                (b"layr", 5, raw_record(5, b"symm", (0, 0, 0, 0), b"norm", 100, [0, 0, 1, 0], "尺子", None)),
                (b"lpix", 2, pixels),
                (b"lpix", 3, white()),
                (b"lpix", 6, white()),
                (b"mpix", 9, mask()),
                (b"mpix", 10, mask()),
            ],
        );
        let (doc, notes) = load_bytes(&bytes).unwrap();
        // 辅助图层跳过并说明；其余自下而上，组里的两层深度 1。
        assert_eq!(notes.len(), 1, "{notes:?}");
        let names: Vec<(String, u16)> = doc.layers().iter().map(|l| (l.props.name.clone(), l.depth)).collect();
        assert_eq!(names, [("Layer".to_string(), 0), ("挡住".into(), 1), ("显示".into(), 1), ("组".into(), 0)]);
        let (layer, hide, show, group) = (&doc.layers()[0], &doc.layers()[1], &doc.layers()[2], &doc.layers()[3]);
        assert!(group.is_folder() && group.props.pass_through);
        assert_eq!((layer.props.blend, layer.props.lock_alpha, layer.props.clip), (BlendMode::Brighten, true, true));
        assert_eq!((hide.props.blend, hide.props.opacity), (BlendMode::Multiply, 0.5));
        // 压缩块逐个像素还原（乘 2 成 15 位；文件里是 B、G、R、A，分块里是 R、G、B、A）。
        for (x, y) in [(0, 0), (5, 3), (31, 31), (17, 9)] {
            let k = (y * 32 + x) as usize;
            let want = [ramp(2)[k], ramp(1)[k], ramp(0)[k], FULL].map(|v| (v * 2) as u16);
            assert_eq!(pixel(&layer.tiles, x, y), want, "({x}, {y})");
        }
        assert_eq!(pixel(&layer.tiles, 40, 10), [0x6000, 0x4000, 0x2000, 0x8000]);
        assert_eq!(pixel(&layer.tiles, 70, 10), [0; 4]);
        assert_eq!(pixel(&layer.tiles, 10, 40), [0; 4]);
        // 蒙版：标志第一个字节是 1 的，值是"挡住多少"，范围外显示；是 0 的，值是"显示多少"，范围外挡住。
        let value = |l: &Layer, x: i32, y: i32| {
            let m = l.mask.as_ref().unwrap();
            m.tile((x.div_euclid(TILE), y.div_euclid(TILE))).map_or(m.default, |t| t[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize])
        };
        let one = ONE as u16;
        assert_eq!((value(hide, 33, 5), value(hide, 50, 5), value(hide, 10, 5)), (0, one, one));
        assert_eq!((value(show, 33, 5), value(show, 50, 5), value(show, 10, 5)), (one, 0, 0));
    }

    #[test]
    fn rejects_broken_files() {
        let error = |bytes: &[u8]| load_bytes(bytes).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error(b"not a canvas file at all, definitely not").contains("文件头"));
        let one = |strip: Vec<u8>| file(64, 32, &[(b"layr", 2, raw_record(2, b"norm", (0, 0, 1, 1), b"norm", 100, [0, 0, 1, 0], "x", None)), (b"lpix", 2, dpcm(&[strip]))]);
        // 块的标记里的列号不对。
        assert!(error(&one(vec![0xff, 0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xf1])).contains("块的标记"));
        // 压缩块的数据不够。
        assert!(error(&one(strip(0xff, 0, &[Piece::Coded(vec![0x80])]))).contains("压缩块"));
        // 尺寸太大。
        let mut big = file(64, 32, &[]);
        big[20..24].copy_from_slice(&(MAX_SIDE as u32 + 1).to_le_bytes());
        assert!(error(&big).contains("不支持"));
    }

    // ---- 写 ----

    use crate::document::{Artboard, Fill};
    use crate::ora::shared::testing::{pixel as pixel_of, set_pixel, Rng, Temp};

    /// 存了再读回来。返回读回来的文档、存的时候的说明和文件的字节。
    fn round_trip(doc: &Document) -> (Document, Vec<String>, Vec<u8>) {
        let dir = Temp::new("sai2");
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("t.sai2");
        let notes = save(doc, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let (back, read_notes) = load(&path).unwrap();
        assert!(read_notes.is_empty(), "{read_notes:?}");
        (back, notes, bytes)
    }

    /// 存进文件、再读回来以后该有的像素：16 位里放不下 15 位的最低位。
    fn after(px: [u16; 4]) -> [u16; 4] {
        let a = to16(px[3]);
        let c = |v: u16| (to16(v).min(a) * 2) as u16;
        [c(px[0]), c(px[1]), c(px[2]), (a * 2) as u16]
    }

    fn mask_value(m: &Mask, x: i32, y: i32) -> u16 {
        m.tile((x.div_euclid(TILE), y.div_euclid(TILE))).map_or(m.default, |t| t[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize])
    }

    fn set_mask(m: &mut Mask, x: i32, y: i32, v: u16) {
        let default = m.default;
        m.tiles.get_mut((x.div_euclid(TILE), y.div_euclid(TILE)), default)[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize] = v;
    }

    /// 图层、组（穿透、隔离、套两层、展开）、蒙版（默认白、默认黑、停用、组上的）、剪贴、保护不透明度、隐藏、
    /// 31 种混合模式、伸出画布、空图层、超长的名字都有；像素有随机的、整块一种颜色的、隔着空块的。
    fn rich_document() -> Document {
        let (w, h) = (200, 130);
        let mut rng = Rng(0x5A12_0001);
        let mut bottom = Layer::new("底 bottom");
        for y in 0..h {
            for x in -40..w {
                set_pixel(&mut bottom, x, y, rng.pixel());
            }
        }
        // 整块一种颜色的（对齐到 32），隔一块空的再来一块。
        let mut solid = Layer::new("一种颜色");
        for y in 64..96 {
            for x in (32..64).chain(96..128) {
                set_pixel(&mut solid, x, y, [1000, 2000, 3001, 9000]);
            }
        }
        solid.props.clip = true;
        let mut hidden = Layer::new("藏起来");
        hidden.props.visible = false;
        set_pixel(&mut hidden, 3, 3, [ONE as u16; 4]);
        let mut locked = Layer::new("保护");
        locked.props.lock_alpha = true;
        for x in 0..w {
            set_pixel(&mut locked, x, 100, rng.pixel());
        }
        let mut a = Layer::folder("组 A");
        a.props = LayerProps { pass_through: false, blend: BlendMode::Multiply, opacity: 0.8, ..a.props };
        a.kind = Kind::Folder { open: true };
        let mut mask = Mask::white();
        for y in 10..50 {
            for x in 20..90 {
                set_mask(&mut mask, x, y, ((x * 300 + y * 7) % (ONE as i32 + 1)) as u16);
            }
        }
        a.mask = Some(mask);
        let mut deep = Layer::new("深");
        for y in 0..40 {
            for x in 150..230 {
                set_pixel(&mut deep, x, y, rng.pixel());
            }
        }
        let mut inner = Layer::folder("里");
        inner.kind = Kind::Folder { open: false };
        let mut b = Layer::folder("组 B");
        b.kind = Kind::Folder { open: false };
        let mut dark = Layer::new("黑蒙版");
        for y in 20..60 {
            for x in 20..60 {
                set_pixel(&mut dark, x, y, [5000, 6000, 7000, 8000]);
            }
        }
        let mut black = Mask { tiles: GrayMap::default(), default: 0, disabled: true };
        for y in 30..40 {
            for x in 30..70 {
                set_mask(&mut black, x, y, (x * 400) as u16);
            }
        }
        dark.mask = Some(black);
        let mut layers = vec![bottom, solid, hidden, locked, a, deep, inner, b, dark];
        for (d, depth) in [(1, 1), (2, 1), (3, 1), (5, 2), (6, 1)] {
            layers[d].depth = depth;
        }
        for (k, mode) in BlendMode::ALL.into_iter().chain(BlendMode::EXTRA).enumerate() {
            let mut l = Layer::new(&format!("{k} {mode:?}"));
            l.props = LayerProps { blend: mode, opacity: (k % 10) as f32 / 10.0 + 0.05, ..l.props };
            let (x0, y0) = ((k as i32 * 13) % w, (k as i32 * 7) % h);
            for y in y0..y0 + 9 {
                for x in x0..x0 + 9 {
                    set_pixel(&mut l, x, y, rng.pixel());
                }
            }
            layers.push(l);
        }
        layers.push(Layer::new("空"));
        let mut long = Layer::new(&"长".repeat(300));
        set_pixel(&mut long, w - 1, h - 1, [1, 1, 1, 1]);
        layers.push(long);
        let mut doc = Document::from_layers(w, h, [255; 3], layers);
        doc.set_active(5);
        doc
    }

    #[test]
    fn writes_what_it_reads() {
        let doc = rich_document();
        let (back, notes, _) = round_trip(&doc);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!((back.width, back.height, back.layers().len(), back.active()), (doc.width, doc.height, doc.layers().len(), 5));
        let keep = doc.keep_bounds();
        for (a, b) in doc.layers().iter().zip(back.layers()) {
            let (p, q) = (&a.props, &b.props);
            let name: String = p.name.chars().take(255).collect();
            assert_eq!(q.name, name);
            assert_eq!((a.depth, a.kind.clone(), p.visible, p.lock_alpha, p.clip), (b.depth, b.kind.clone(), q.visible, q.lock_alpha, q.clip), "{name}");
            assert_eq!((q.opacity * 100.0).round(), (p.opacity * 100.0).round(), "{name}");
            if a.is_folder() {
                assert_eq!(q.pass_through, p.pass_through, "{name}");
            }
            if !(a.is_folder() && p.pass_through) {
                assert_eq!(q.blend, p.blend, "{name}");
            }
            for y in keep.y0..keep.y1 {
                for x in keep.x0..keep.x1 {
                    assert_eq!(pixel_of(b, x, y), after(pixel_of(a, x, y)), "{name} ({x}, {y})");
                }
            }
            assert_eq!(a.mask.is_some(), b.mask.is_some(), "{name}");
            if let (Some(m), Some(n)) = (&a.mask, &b.mask) {
                assert_eq!((n.default, n.disabled), (m.default, m.disabled), "{name}");
                for y in keep.y0..keep.y1 {
                    for x in keep.x0..keep.x1 {
                        let v = mask_value(m, x, y);
                        let want = if hides(m) { ONE as u16 - 2 * to16(ONE as u16 - v) as u16 } else { 2 * to16(v) as u16 };
                        assert_eq!(mask_value(n, x, y), want, "{name} ({x}, {y})");
                    }
                }
            }
        }
    }

    /// 读一个我们写的文件的块表：(类型, 编号, 数据)，按块表的顺序。
    fn chunks_of(bytes: &[u8]) -> Vec<([u8; 4], u32, &[u8])> {
        let n = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
        let entries: Vec<([u8; 4], u32, usize)> = (0..n)
            .map(|i| {
                let e = &bytes[64 + 16 * i..80 + 16 * i];
                (e[0..4].try_into().unwrap(), u32::from_le_bytes(e[4..8].try_into().unwrap()), u64::from_le_bytes(e[8..16].try_into().unwrap()) as usize)
            })
            .collect();
        (0..n).map(|i| (entries[i].0, entries[i].1, &bytes[entries[i].2..entries.get(i + 1).map_or(bytes.len(), |e| e.2)])).collect()
    }

    #[test]
    fn file_looks_like_the_reference_files() {
        let doc = rich_document();
        let (_, _, bytes) = round_trip(&doc);
        let u32_at = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        assert_eq!(&bytes[..16], SIGNATURE);
        assert_eq!((u32_at(&bytes, 16), u32_at(&bytes, 20), u32_at(&bytes, 24), u32_at(&bytes, 28)), (HEAD_FLAGS, 200, 130, HEAD_1C));
        // 当前图层是第 5 层，编号 2 + 5。
        assert_eq!(u32_at(&bytes, 36), 7);
        assert_eq!(&bytes[40..64], &[[0u8; 16].as_slice(), &[0x80, 0x80, 0x80, 0xFF], b"norm"].concat()[..]);
        let chunks = chunks_of(&bytes);
        // 顺序：hist、intg、图层记录（从上往下）、各层的像素（从上往下，先 lpix 后 mpix）。
        let n = doc.layers().len() as u32;
        let order: Vec<String> = chunks.iter().map(|(t, id, _)| format!("{}{id}", String::from_utf8_lossy(t))).collect();
        let mut want = vec!["hist0".to_string(), "intg0".into()];
        want.extend((0..n).rev().map(|i| format!("layr{}", i + 2)));
        // 蒙版：组 A（第 4 层）在下，编号 2 + n；黑蒙版（第 8 层）在上，编号 3 + n。
        for i in (0..n).rev() {
            if !doc.layers()[i as usize].is_folder() {
                want.push(format!("lpix{}", i + 2));
            }
            match i {
                4 => want.push(format!("mpix{}", n + 2)),
                8 => want.push(format!("mpix{}", n + 3)),
                _ => {}
            }
        }
        assert_eq!(order, want);
        // 每块数据补齐到 4 字节；hist 是 88 字节，时间后面补 0 到 64 字节，最后是 1。
        assert!(chunks.iter().all(|(_, _, d)| d.len() % 4 == 0));
        let hist = chunks[0].2;
        let text = String::from_utf16(&hist[..64].as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u)).take_while(|&u| u != 0).collect::<Vec<_>>()).unwrap();
        assert!(text.len() == 17 && text.ends_with(".2") && text.as_bytes()[8] == b'.', "{text}");
        assert_eq!((hist.len(), &hist[80..88]), (88, &1u64.to_le_bytes()[..]));
        // 图层记录：8、12、20、24 是 0；名字补齐到 4 字节；最长 255 个字。
        for (_, id, rec) in chunks.iter().filter(|c| &c.0 == b"layr") {
            assert_eq!(u32_at(rec, 4), *id);
            assert!([8, 12, 20, 24].iter().all(|&at| u32_at(rec, at) == 0));
            assert_eq!(&rec[56..60], b"name");
            assert_eq!(u32_at(rec, 60) % 4, 0);
        }
        let long = chunks.iter().find(|c| &c.0 == b"layr" && c.1 == n + 1).unwrap().2;
        assert_eq!(u16::from_le_bytes([long[64], long[65]]), 255);
        // 像素：每条以类型 F 结束，列号是范围的右边；没有透明的类型 5。
        for (tag, id, data) in chunks.iter().filter(|c| &c.0 == b"lpix") {
            let rec = chunks.iter().find(|c| &c.0 == b"layr" && c.1 == *id).unwrap().2;
            let (bx, bw, bh) = (u32_at(rec, 28) as i32, u32_at(rec, 36) as i32, u32_at(rec, 40) as usize);
            if bh == 0 {
                assert_eq!(*data, &[0u8; 4][..]);
                continue;
            }
            let mut at = 4 + 4 * bh;
            for k in 0..bh {
                let s = &data[at..at + u32_at(data, 4 + 4 * k) as usize];
                at += s.len();
                assert_eq!(s[s.len() - 2..], [MARK, 0xF0 | (bx + bw).rem_euclid(16) as u8], "{} {id}", String::from_utf8_lossy(tag));
                let mut p = 0;
                while s[p + 1] >> 4 != 0xF {
                    match s[p + 1] >> 4 {
                        0 => p += 4,
                        5 => {
                            assert_ne!(&s[p + 8..p + 10], &[0, 0]);
                            p += 10;
                        }
                        _ => p += 4 + u16::from_le_bytes([s[p + 2], s[p + 3]]) as usize,
                    }
                }
            }
        }
        // 一种颜色的那层：一条里是 类型 5、类型 0（空一块）、类型 5，再 F。
        let solid = chunks.iter().find(|c| &c.0 == b"lpix" && c.1 == 3).unwrap().2;
        assert_eq!(&solid[8..8 + 2], &[MARK, 0x51]);
        assert_eq!(&solid[18..22], &[MARK, 0x02, 0, 0]);
    }

    #[test]
    fn merged_image_is_our_composite() {
        let doc = rich_document();
        let dir = Temp::new("sai2-merged");
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("m.sai2");
        save(&doc, &path).unwrap();
        let (w, h, rgba, alpha) = merged(&path).unwrap();
        assert_eq!((w, h, alpha), (200, 130, true));
        assert!(rgba == doc.flatten_rgba());
    }

    #[test]
    fn what_the_format_cannot_hold_is_drawn_or_dropped() {
        let (w, h) = (100, 70);
        let mut solid = Layer::new("纯色");
        solid.kind = Kind::Fill(Fill { paint: Paint::Solid([200, 100, 50]), psd: None });
        let mut mask = Mask::white();
        set_mask(&mut mask, 5, 5, 0);
        solid.mask = Some(mask);
        let mut gradient = Layer::new("渐变");
        gradient.kind = Kind::Fill(Fill { paint: Paint::Other, psd: None });
        let mut styled = Layer::new("有样式");
        set_pixel(&mut styled, 1, 1, [ONE as u16; 4]);
        styled.style = Some(crate::style::Style { psd: Vec::new(), effects: Vec::new(), enabled: true, switch_at: None, light: None });
        styled.props = LayerProps { fill: 0.5, opacity: 0.8, shapes: false, ..styled.props };
        let mut board = Layer::folder("画板");
        board.artboard = Some(Artboard { rect: None, background: None, psd: Vec::new() });
        let doc = Document::from_layers(w, h, [10, 20, 30], vec![solid, gradient, styled, board]);
        let (back, notes, _) = round_trip(&doc);
        assert_eq!(notes.len(), 6, "{notes:?}");
        // 背景色不是白的：最底下多一层"背景"，铺满画布。
        let names: Vec<&str> = back.layers().iter().map(|l| l.props.name.as_str()).collect();
        assert_eq!(names, [background_name(), "纯色", "渐变", "有样式", "画板"]);
        let bg = pixel_of(&back.layers()[0], w - 1, h - 1);
        assert_eq!(bg[3], ONE as u16);
        assert!(bg[..3].iter().zip([10u32, 20, 30]).all(|(&v, c)| (v as i64 - (c * ONE / 255) as i64).abs() <= 2), "{bg:?}");
        assert_eq!(pixel_of(&back.layers()[0], w, 0), [0; 4]);
        // 纯色的填充图层画成铺满画布的像素，蒙版照样在。
        let fill = &back.layers()[1];
        assert!(fill.is_pixels() && fill.mask.is_some());
        assert_eq!(pixel_of(fill, 0, 0)[3], ONE as u16);
        assert_eq!(pixel_of(fill, w - 1, h - 1), pixel_of(fill, 0, 0));
        assert_eq!(mask_value(fill.mask.as_ref().unwrap(), 5, 5), 0);
        // 画不出来的是空的；填充不透明度乘进不透明度；画板是普通的组。
        assert!(back.layers()[2].is_pixels() && back.layers()[2].tiles.is_empty());
        assert_eq!(back.layers()[3].props.opacity, 0.4);
        assert!(back.layers()[4].is_folder());
        // 背景色是白的就不加图层。
        let white = Document::from_layers(w, h, [255; 3], vec![Layer::new("一")]);
        assert_eq!(round_trip(&white).0.layers().len(), 1);
    }

    #[test]
    fn encoder_handles_the_extremes() {
        // 差最大（0 和 0x4000 交替）、很长的一串 0、一串正好 8 个和 7 个 0。
        let mut v = vec![0i32; SAMPLES];
        for (k, x) in v.iter_mut().enumerate() {
            *x = match k {
                0..=63 => if k % 2 == 0 { FULL } else { 0 },
                64..=71 => 0,
                72..=78 => 5,
                _ => 0,
            };
        }
        let mut bits = Bits { data: &coded_block(&[v.clone()]), pos: 0 };
        let mut back = [0i32; SAMPLES];
        residuals(&mut bits, &mut back).unwrap();
        unpredict(&mut back, BLOCK, FULL);
        assert_eq!(back.to_vec(), v);
        // 合并图像：画布宽高不是 256 的倍数，两行两列格子。
        let (w, h) = (300, 260);
        let mut rng = Rng(7);
        let rgba: Vec<u8> = (0..w * h).flat_map(|_| rng.pixel8()).collect();
        let dir = Temp::new("sai2-intg");
        let bytes = file(w as u32, h as u32, &[(b"intg", 0, encode_merged(w, h, &rgba))]);
        std::fs::write(&dir.0, bytes).unwrap();
        let (_, _, got, alpha) = merged(&dir.0).unwrap();
        assert!(alpha && got == rgba);
    }

    /// 混合模式的边角：八种模式（色相、饱和度、颜色、明度、深色、浅色、亮光、实色混合）各一个不透明的图层，
    /// 14 种底色 × 14 种源色，每格 4×4 像素，都是 8 位里正好的值。颜色里有灰（没有色相）、0 和 255 的分量、
    /// "加起来"和"亮度"比出来不一样的两种颜色（实现方案 §3.17 的注）。
    fn edge_document() -> Document {
        const COLORS: [[u8; 3]; 14] = [
            [0, 0, 0],
            [255, 255, 255],
            [128, 128, 128],
            [64, 64, 64],
            [255, 0, 0],
            [0, 0, 255],
            [0, 255, 0],
            [60, 60, 60],
            [200, 100, 50],
            [10, 200, 240],
            [255, 255, 0],
            [128, 0, 255],
            [1, 254, 128],
            [127, 128, 129],
        ];
        const MODES: [BlendMode; 8] = [
            BlendMode::Hue,
            BlendMode::Saturation,
            BlendMode::Color,
            BlendMode::Luminosity,
            BlendMode::DarkerColor,
            BlendMode::LighterColor,
            BlendMode::VividLight,
            BlendMode::HardMix,
        ];
        let (cell, side) = (4, 4 * COLORS.len() as i32);
        let px = |c: [u8; 3]| [c[0], c[1], c[2]].map(|v| ((v as u32 * ONE + 127) / 255) as u16);
        let mut bottom = Layer::new("底");
        let mut layers = Vec::new();
        for (k, mode) in MODES.into_iter().enumerate() {
            let (ox, oy) = ((k as i32 % 4) * side, (k as i32 / 4) * side);
            let mut l = Layer::new(&format!("{mode:?}"));
            l.props.blend = mode;
            for (i, b) in COLORS.iter().enumerate() {
                for (j, s) in COLORS.iter().enumerate() {
                    for y in 0..cell {
                        for x in 0..cell {
                            let (x, y) = (ox + j as i32 * cell + x, oy + i as i32 * cell + y);
                            let [r, g, bl] = px(*b);
                            set_pixel(&mut bottom, x, y, [r, g, bl, ONE as u16]);
                            let [r, g, bl] = px(*s);
                            set_pixel(&mut l, x, y, [r, g, bl, ONE as u16]);
                        }
                    }
                }
            }
            layers.push(l);
        }
        layers.insert(0, bottom);
        Document::from_layers(4 * side, 2 * side, [255; 3], layers)
    }

    /// 给参照软件打开用的文件（内容都是我们生成的）：`ZHIBI_SAI2_SAMPLES` 里的 blocks.png、layers.psd、modes.psd
    /// （参照软件以前从它们转存过样本）各存一份，再加上 `rich_document`。写到 `ZHIBI_SAI2_OUT` 目录。
    #[test]
    #[ignore]
    fn write_files_for_the_reference() {
        let samples = PathBuf::from(std::env::var("ZHIBI_SAI2_SAMPLES").expect("用 ZHIBI_SAI2_SAMPLES 指定样本目录"));
        let out = PathBuf::from(std::env::var("ZHIBI_SAI2_OUT").expect("用 ZHIBI_SAI2_OUT 指定输出目录"));
        std::fs::create_dir_all(&out).unwrap();
        let blocks = crate::image::load(&samples.join("blocks.png")).unwrap().0;
        let layers = crate::psd::load(&samples.join("layers.psd")).unwrap().0;
        let modes = crate::psd::load(&samples.join("modes.psd")).unwrap().0;
        for (name, doc) in [("blocks", blocks), ("layers", layers), ("modes", modes), ("rich", rich_document()), ("edges", edge_document())] {
            let notes = save(&doc, &out.join(format!("{name}.sai2"))).unwrap();
            eprintln!("{name}: {} 层，说明 {notes:?}", doc.layers().len());
        }
    }

    /// 两个文件的图层是不是一样（个数、属性、像素、蒙版），只出数字。参照软件打开我们写的文件再存一次，拿来和我们写的比。
    /// `ZHIBI_SAI2_COMPARE` 是一个，`ZHIBI_SAI2_OTHER` 是另一个。
    #[test]
    #[ignore]
    fn same_layers_as_other() {
        let a = load(&PathBuf::from(std::env::var("ZHIBI_SAI2_COMPARE").unwrap())).unwrap().0;
        let b = load(&PathBuf::from(std::env::var("ZHIBI_SAI2_OTHER").unwrap())).unwrap().0;
        eprintln!("{} 层 / {} 层，当前图层 {} / {}", a.layers().len(), b.layers().len(), a.active(), b.active());
        let keep = a.keep_bounds();
        for (i, (x, y)) in a.layers().iter().zip(b.layers()).enumerate() {
            let (p, q) = (&x.props, &y.props);
            let props = (&p.name, p.opacity, p.visible, p.blend, p.lock_alpha, p.clip, p.pass_through, x.depth, &x.kind)
                == (&q.name, q.opacity, q.visible, q.blend, q.lock_alpha, q.clip, q.pass_through, y.depth, &y.kind);
            let mut pixels = 0;
            let mut masks = 0;
            for py in keep.y0..keep.y1 {
                for px in keep.x0..keep.x1 {
                    pixels += (pixel_of(x, px, py) != pixel_of(y, px, py)) as usize;
                    if let (Some(m), Some(n)) = (&x.mask, &y.mask) {
                        masks += (mask_value(m, px, py) != mask_value(n, px, py)) as usize;
                    }
                }
            }
            let mask_kind = (x.mask.as_ref().map(|m| (m.default, m.disabled)), y.mask.as_ref().map(|m| (m.default, m.disabled)));
            if !props || pixels > 0 || masks > 0 || mask_kind.0 != mask_kind.1 {
                eprintln!("  #{i} {:?}：属性一样 {props}，不同的像素 {pixels}，蒙版 {mask_kind:?} 不同的值 {masks}", p.name);
                if !props {
                    eprintln!("     {:?} {} {} {:?} {} {} {} {} {:?}", p.name, p.opacity, p.visible, p.blend, p.lock_alpha, p.clip, p.pass_through, x.depth, x.kind);
                    eprintln!("     {:?} {} {} {:?} {} {} {} {} {:?}", q.name, q.opacity, q.visible, q.blend, q.lock_alpha, q.clip, q.pass_through, y.depth, y.kind);
                }
            }
        }
    }

    /// 读用户的一个文件、存成这种格式、再读回来：图层、属性、像素、蒙版都应该一样（文件里的值是偶数，来回一位不差）。
    /// 只出数字。`ZHIBI_SAI2_COMPARE` 指文件；`ZHIBI_SAI2_OUT` 指定写到哪里（不给就写到临时目录，用完删掉）。
    #[test]
    #[ignore]
    fn rewrite_user_file() {
        let path = PathBuf::from(std::env::var("ZHIBI_SAI2_COMPARE").expect("用 ZHIBI_SAI2_COMPARE 指定文件"));
        let started = std::time::Instant::now();
        let (doc, _) = load(&path).unwrap();
        let loaded = started.elapsed();
        let temp = Temp::new("sai2-rewrite.sai2");
        let out = std::env::var("ZHIBI_SAI2_OUT").map(PathBuf::from).unwrap_or_else(|_| temp.0.clone());
        let started = std::time::Instant::now();
        let notes = save(&doc, &out).unwrap();
        let saved = started.elapsed();
        let (back, _) = load(&out).unwrap();
        let size = |p: &Path| std::fs::metadata(p).unwrap().len() as f64 / 1e6;
        eprintln!(
            "{}×{}，{} 层；读 {:.0} 毫秒，写 {:.0} 毫秒；原文件 {:.1} MB，写出 {:.1} MB；说明 {notes:?}",
            doc.width,
            doc.height,
            doc.layers().len(),
            loaded.as_secs_f64() * 1000.0,
            saved.as_secs_f64() * 1000.0,
            size(&path),
            size(&out)
        );
        assert_eq!((back.layers().len(), back.active()), (doc.layers().len(), doc.active()));
        let same_tiles = |a: &TileMap, b: &TileMap| {
            a.coords().chain(b.coords()).all(|tc| {
                let get = |m: &TileMap| m.get(tc).map(|t| t.0.clone()).unwrap_or_else(|| vec![0; (TILE * TILE * 4) as usize]);
                get(a) == get(b)
            })
        };
        let mut wrong = Vec::new();
        for (i, (a, b)) in doc.layers().iter().zip(back.layers()).enumerate() {
            let (p, q) = (&a.props, &b.props);
            let props_same = (&p.name, p.opacity, p.visible, p.blend, p.lock_alpha, p.clip, p.pass_through, a.depth, &a.kind)
                == (&q.name, q.opacity, q.visible, q.blend, q.lock_alpha, q.clip, q.pass_through, b.depth, &b.kind);
            let mask_same = match (&a.mask, &b.mask) {
                (None, None) => true,
                (Some(m), Some(n)) => {
                    let keep = doc.keep_bounds();
                    (m.default, m.disabled) == (n.default, n.disabled)
                        && m.tiles.coords().chain(n.tiles.coords()).all(|(tx, ty)| {
                            (0..TILE * TILE).all(|k| {
                                let (x, y) = (tx * TILE + k % TILE, ty * TILE + k / TILE);
                                !(keep.x0..keep.x1).contains(&x) || !(keep.y0..keep.y1).contains(&y) || mask_value(m, x, y) == mask_value(n, x, y)
                            })
                        })
                }
                _ => false,
            };
            if !props_same || !mask_same || !same_tiles(&a.tiles, &b.tiles) {
                wrong.push(format!("#{i} 属性 {props_same} 蒙版 {mask_same}"));
            }
        }
        assert!(wrong.is_empty(), "不一样的图层：{wrong:?}");
        let (_, _, rgba, _) = merged(&out).unwrap();
        assert!(rgba == back.flatten_rgba(), "写进去的合并图像不是我们的合成结果");
    }

    /// 和文件里存的合并图像对照：只出数字（差的分布），不看画面。`ZHIBI_SAI2_COMPARE` 指文件。
    #[test]
    #[ignore]
    fn compare_with_merged() {
        let path = PathBuf::from(std::env::var("ZHIBI_SAI2_COMPARE").expect("用 ZHIBI_SAI2_COMPARE 指定文件"));
        if let Ok(m) = std::env::var("ZHIBI_CLIP_MODEL") {
            crate::document::CLIP_MODEL.store(m.parse().unwrap(), std::sync::atomic::Ordering::Relaxed);
        }
        let started = std::time::Instant::now();
        let (doc, notes) = load(&path).unwrap();
        let loaded = started.elapsed();
        let (w, h, want, has_alpha) = merged(&path).unwrap();
        assert_eq!((w, h), (doc.width, doc.height));
        // 旧文件的合并图像没有透明度，是垫着白底的：我们这边也垫上白底再比。
        let got = if has_alpha {
            doc.flatten_rgba()
        } else {
            doc.flatten_rgb().as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect()
        };
        let (mut within2, mut over10, mut over40, mut worst) = (0usize, 0usize, 0usize, 0i32);
        for (g, m) in got.as_chunks::<4>().0.iter().zip(want.as_chunks::<4>().0.iter()) {
            // 透明度低的地方颜色本来就不准，按透明度加权看颜色差。
            let alpha_diff = (g[3] as i32 - m[3] as i32).abs();
            let weight = g[3].min(m[3]) as i32;
            let colour_diff = (0..3).map(|c| (g[c] as i32 - m[c] as i32).abs() * weight / 255).max().unwrap();
            let d = alpha_diff.max(colour_diff);
            worst = worst.max(d);
            within2 += (d <= 2) as usize;
            over10 += (d > 10) as usize;
            over40 += (d > 40) as usize;
        }
        let n = (w * h) as f64;
        eprintln!("{}×{}, {} 层，读入 {:.0} 毫秒", w, h, doc.layers().len(), loaded.as_secs_f64() * 1000.0);
        eprintln!("差 ≤2：{:.2}%，>10：{:.3}%，>40：{:.3}%，最大 {worst}", 100.0 * within2 as f64 / n, 100.0 * over10 as f64 / n, 100.0 * over40 as f64 / n);
        // 差 >10 的像素落在哪里、那里有哪些看得见的图层（只出数字和图层名）。
        let mut bad = Rect::EMPTY;
        let mut count = 0usize;
        for (k, (g, m)) in got.as_chunks::<4>().0.iter().zip(want.as_chunks::<4>().0.iter()).enumerate() {
            if (0..4).any(|c| (g[c] as i32 - m[c] as i32).abs() > 10) {
                let (x, y) = ((k % w as usize) as i32, (k / w as usize) as i32);
                bad = bad.union(Rect { x0: x, y0: y, x1: x + 1, y1: y + 1 });
                count += 1;
            }
        }
        eprintln!("差 >10 的范围：{bad:?}，{count} 个像素；合并图像有透明度：{has_alpha}");
        // 差 >10 的像素里：透明度差多少、颜色差多少（各自的平均），以及几个样本。
        let (mut da, mut dc, mut shown) = (0i64, 0i64, 0);
        for (k, (g, m)) in got.as_chunks::<4>().0.iter().zip(want.as_chunks::<4>().0.iter()).enumerate() {
            if (0..4).any(|c| (g[c] as i32 - m[c] as i32).abs() > 10) {
                da += (g[3] as i64 - m[3] as i64).abs();
                dc += (0..3).map(|c| (g[c] as i64 - m[c] as i64).abs()).max().unwrap();
                if shown < 12 && k % 997 == 0 {
                    eprintln!("    ({}, {}) 我们 {:?} 文件 {:?}", k % w as usize, k / w as usize, g, m);
                    shown += 1;
                }
            }
        }
        if count > 0 {
            eprintln!("    平均：透明度差 {:.1}，颜色差 {:.1}", da as f64 / count as f64, dc as f64 / count as f64);
        }
        for (i, l) in doc.layers().iter().enumerate() {
            let b = shared::content_bounds(&l.tiles, doc.keep_bounds());
            let hit = b.intersect(bad);
            eprintln!(
                "  #{i:3} 深{} {}{:?} {:.0}% {}{}{} 范围 {:?}{}",
                l.depth,
                if l.is_folder() { "组 " } else { "" },
                l.props.blend,
                l.props.opacity * 100.0,
                if l.props.visible { "" } else { "隐藏 " },
                if l.props.clip { "剪贴 " } else { "" },
                if l.mask.is_some() { "蒙版 " } else { "" },
                b,
                if hit.is_empty() { "" } else { " ←" }
            );
        }
        let mut modes: Vec<String> = doc.layers().iter().filter(|l| l.props.visible).map(|l| format!("{:?}", l.props.blend)).collect();
        modes.sort();
        modes.dedup();
        eprintln!("用到的混合模式：{modes:?}");
        for note in notes {
            eprintln!("说明：{note}");
        }
    }

    /// 找出画错的图层：每次藏起一层，看和合并图像差 >10 的像素少了多少（只出数字）。
    #[test]
    #[ignore]
    fn blame_layers() {
        let path = PathBuf::from(std::env::var("ZHIBI_SAI2_COMPARE").unwrap());
        let (mut doc, _) = load(&path).unwrap();
        let (_, _, want, has_alpha) = merged(&path).unwrap();
        let count = |doc: &Document| {
            let got = if has_alpha { doc.flatten_rgba() } else { doc.flatten_rgb().as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect() };
            got.as_chunks::<4>().0.iter().zip(want.as_chunks::<4>().0.iter()).filter(|(g, m)| (0..4).any(|c| (g[c] as i32 - m[c] as i32).abs() > 10)).count()
        };
        let base = count(&doc);
        eprintln!("全部：差 >10 的 {base} 个像素");
        for i in 0..doc.layers().len() {
            let p = doc.layers()[i].props.clone();
            if !p.visible {
                continue;
            }
            doc.set_props_live(i, LayerProps { visible: false, ..p.clone() });
            let n = count(&doc);
            doc.set_props_live(i, p);
            if n < base * 8 / 10 || n > base * 3 {
                let l = &doc.layers()[i];
                eprintln!("  藏起 #{i} 深{} {}{:?}{}：{n}", l.depth, if l.is_folder() { "组 " } else { "" }, l.props.blend, if l.mask.is_some() { " 蒙版" } else { "" });
            }
        }
    }

    /// 某一点上每个图层的像素（0–255，非预乘）、蒙版的值、属性。`ZHIBI_POINTS="x,y;x,y"`。
    #[test]
    #[ignore]
    fn layers_at_points() {
        let path = PathBuf::from(std::env::var("ZHIBI_SAI2_COMPARE").unwrap());
        let (doc, _) = load(&path).unwrap();
        let (w, _, want, _) = merged(&path).unwrap();
        for pt in std::env::var("ZHIBI_POINTS").unwrap().split(';') {
            let v: Vec<i32> = pt.split(',').map(|n| n.trim().parse().unwrap()).collect();
            let (x, y) = (v[0], v[1]);
            let o = (y as usize * w as usize + x as usize) * 4;
            eprintln!("({x}, {y}) 文件 {:?}", &want[o..o + 3]);
            for (i, l) in doc.layers().iter().enumerate() {
                let mut px = [0u8; 4];
                shared::read_row(&l.tiles, y, x, x + 1, &mut px);
                let m = l.mask.as_ref().map(|m| m.tile((x.div_euclid(TILE), y.div_euclid(TILE))).map_or(m.default, |t| t[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize]));
                if px[3] > 0 || l.is_folder() || m.is_some() {
                    eprintln!(
                        "  #{i:3} 深{} {}{:?} {:.0}%{}{}{} 像素 {:?} 蒙版 {:?}",
                        l.depth,
                        if l.is_folder() { "组 " } else { "" },
                        l.props.blend,
                        l.props.opacity * 100.0,
                        if l.props.visible { "" } else { " 隐藏" },
                        if l.props.clip { " 剪贴" } else { "" },
                        if l.is_folder() && l.props.pass_through { " 穿透" } else { "" },
                        px,
                        m.map(|v| v as u32 * 255 / ONE)
                    );
                }
            }
        }
    }

    /// 拟合混合模式用：第 `ZHIBI_SAI2_PROBE` 层在哪些像素上是它上面唯一有内容的图层，列出那些像素的
    ///（下面合成好的颜色、这一层的颜色和透明度、文件里合并图像的颜色），都是 0–255 的数字。
    #[test]
    #[ignore]
    fn probe_blend() {
        let path = PathBuf::from(std::env::var("ZHIBI_SAI2_COMPARE").unwrap());
        let li: usize = std::env::var("ZHIBI_SAI2_PROBE").unwrap().parse().unwrap();
        let (mut doc, _) = load(&path).unwrap();
        let (w, _, want, _) = merged(&path).unwrap();
        let n = doc.layers().len();
        let at = |doc: &Document, i: usize, x: i32, y: i32| -> [u8; 4] {
            let mut out = [0u8; 4];
            shared::read_row(&doc.layers()[i].tiles, y, x, x + 1, &mut out);
            out
        };
        // 下面的：把这一层和上面的都藏起来再合成。
        let props: Vec<LayerProps> = doc.layers().iter().map(|l| l.props.clone()).collect();
        for (j, p) in props.iter().enumerate().skip(li) {
            doc.set_props_live(j, LayerProps { visible: false, ..p.clone() });
        }
        let below = doc.flatten_rgb();
        for (j, p) in props.iter().enumerate().skip(li) {
            doc.set_props_live(j, p.clone());
        }
        let b = shared::content_bounds(&doc.layers()[li].tiles, doc.bounds());
        let mut shown = 0;
        let mut y = b.y0;
        while y < b.y1 && shown < 300 {
            let mut x = b.x0;
            while x < b.x1 && shown < 300 {
                let s = at(&doc, li, x, y);
                // 只有最底下那层和这一层有内容的地方：下面的颜色就是最底下那层的，不受别的混合模式影响。
                let alone = (1..n).filter(|&j| j != li).all(|j| !doc.layers()[j].props.visible || at(&doc, j, x, y)[3] == 0);
                if s[3] > 0 && alone {
                    let o = (y as usize * w as usize + x as usize) * 4;
                    let k = (y as usize * doc.width as usize + x as usize) * 3;
                    eprintln!("下 {:?} 层 {:?} 文件 {:?}", &below[k..k + 3], s, &want[o..o + 3]);
                    shown += 1;
                }
                x += 3;
            }
            y += 3;
        }
        eprintln!("图层 #{li}：{:?} 不透明度 {:.2}", doc.layers()[li].props.blend, doc.layers()[li].props.opacity);
    }

    /// 参照软件存的三个样本（用户在本机生成，`ZHIBI_SAI2_SAMPLES` 指目录）和已知内容逐个像素、逐个属性核对。
    #[test]
    #[ignore]
    fn samples_match_their_known_content() {
        let dir = PathBuf::from(std::env::var("ZHIBI_SAI2_SAMPLES").expect("用 ZHIBI_SAI2_SAMPLES 指定样本目录"));
        // blocks：参照软件把 8 位的 v 读成 floor(v·16384/255)，预乘是 floor(颜色·透明度/16384)。
        let (doc, notes) = load(&dir.join("blocks.sai2")).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        // 直接读 PNG 里的 8 位值（经过本程序的分块会把透明度 1/255 的颜色归并，见 `ora::shared`）。
        let mut reader = png::Decoder::new(std::fs::File::open(dir.join("blocks.png")).unwrap()).read_info().unwrap();
        let mut straight = vec![0u8; reader.output_buffer_size()];
        reader.next_frame(&mut straight).unwrap();
        let f16 = |v: u8| v as u32 * 16384 / 255;
        let mut wrong = 0;
        for y in 0..96i32 {
            for x in 0..256i32 {
                let o = (y as usize * 256 + x as usize) * 4;
                let row = [straight[o], straight[o + 1], straight[o + 2], straight[o + 3]];
                let a = f16(row[3]);
                let want = [f16(row[0]) * a / 16384 * 2, f16(row[1]) * a / 16384 * 2, f16(row[2]) * a / 16384 * 2, a * 2];
                let t = doc.layers()[0].tiles.get((x.div_euclid(TILE), y.div_euclid(TILE)));
                let i = ((y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) * 4) as usize;
                let got = t.map_or([0; 4], |t| [t.0[i] as u32, t.0[i + 1] as u32, t.0[i + 2] as u32, t.0[i + 3] as u32]);
                let want = if row[3] == 0 { [0; 4] } else { want };
                wrong += (got != want) as usize;
            }
        }
        assert_eq!(wrong, 0, "和 blocks.png 不一样的像素");

        // layers：名字、不透明度、混合模式、显隐、剪贴、保护不透明度、组（穿透与否、深度）、蒙版。
        let (doc, notes) = load(&dir.join("layers.sai2")).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let summary: Vec<String> = doc
            .layers()
            .iter()
            .map(|l| {
                let p = &l.props;
                format!(
                    "{}{}|{:?}|{:.2}|{}{}{}{}{}",
                    "·".repeat(l.depth as usize),
                    p.name,
                    p.blend,
                    p.opacity,
                    if p.visible { "" } else { " hidden" },
                    if p.clip { " clip" } else { "" },
                    if p.lock_alpha { " lockα" } else { "" },
                    if l.is_folder() && p.pass_through { " pass" } else { "" },
                    if l.mask.is_some() { " mask" } else { "" },
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                "背景|Normal|1.00|",
                "bottom|Normal|1.00|",
                "multiply 37|Multiply|0.37|",
                "screen|Screen|1.00|",
                "hidden|Normal|1.00| hidden",
                "clipped|Normal|1.00| clip",
                "lock alpha|Normal|1.00| lockα",
                "·inside A overlay|Overlay|1.00|",
                "组 A|Normal|0.80|",
                "·inside B add|Add|1.00|",
                "Group B|Normal|1.00| pass",
                "中文名 mask|Normal|1.00| mask",
            ]
        );
        // 蒙版：x 150..200、y 70..120 挡住，别处显示。
        let mask = doc.layers()[11].mask.as_ref().unwrap();
        let shown = |x: i32, y: i32| mask.tile((x.div_euclid(TILE), y.div_euclid(TILE))).map_or(mask.default, |t| t[(y.rem_euclid(TILE) * TILE + x.rem_euclid(TILE)) as usize]);
        assert_eq!((shown(150, 70), shown(199, 119), shown(200, 70), shown(149, 100), shown(10, 10)), (0, 0, ONE as u16, ONE as u16, ONE as u16));

        // modes：每层的名字里写着它在 PS 里的混合模式编号。
        let (doc, notes) = load(&dir.join("modes.sai2")).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        for l in doc.layers().iter().filter(|l| l.props.name.as_bytes().get(1).is_some_and(u8::is_ascii_digit)) {
            let id = l.props.name.split(' ').nth(1).unwrap();
            assert_eq!(l.props.blend.id(), id, "{}", l.props.name);
        }
        let find = |name: &str| doc.layers().iter().find(|l| l.props.name == name).unwrap();
        assert!(find("mask disabled").mask.as_ref().unwrap().disabled);
        assert_eq!(find("deep").depth, 2);
        assert!(find("outer").mask.is_some());
        let off = crate::ora::shared::content_bounds(&find("off canvas").tiles, doc.keep_bounds());
        assert_eq!(off, Rect { x0: -100, y0: 200, x1: 40, y1: 250 });
    }
}
