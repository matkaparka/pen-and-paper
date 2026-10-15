// SPDX-License-Identifier: GPL-3.0-or-later
//! 参照软件第 2 版的文件（`.sai2`）的读取（规格 F-146，实现方案 §3.21）。只读不写。
//!
//! 文件是一张块表加上一串块。图层记录（`layr`）从上往下排，每层的像素在编号相同的 `lpix` 块里，
//! 蒙版在 `mpix` 块里。像素按 32 行高的横条存，一条里是一串 32×32 的块：全透明的一段、一种颜色的一块、
//! 或者压缩过的一块。压缩块里每个通道 1024 个数，存的是"实际值减预测"，用一种变长码写成位流
//!（每个字节从最低位读起）。值是 16 位、预乘透明度，1.0 是 0x4000。
//!
//! 合并图像（`intg`）也读得出来（`merged`），只拿来核对我们合成的结果。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::document::{BlendMode, Document, Kind, Layer, LayerProps, Mask, MAX_SIDE};
use crate::lang::tr;
use crate::ora::shared::{self, invalid, Notes};
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
    let (w, h, chunks) = read_table(&mut file, size)?;
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
    }
    let mut doc = Document::from_layers(w, h, [255; 3], layers);
    doc.dpi = DEFAULT_DPI;
    Ok((doc, notes.into_vec()))
}

/// 文件头和块表。返回画布的宽、高和各块（按在文件里的位置排好，各自知道到哪里结束）。
fn read_table(file: &mut File, size: u64) -> io::Result<(i32, i32, Vec<Chunk>)> {
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
    Ok((w as i32, h as i32, chunks))
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

/// 混合模式的编号。参照软件自己特有的五种（`add `、`sub `、`sbad`、`ddge`、`burn`）是本程序的
/// 提亮、压暗、提亮/压暗、柔和减淡、柔和加深（公式是拿用户文件里的合并图像拟合出来的，见实现方案 §3.21）。
fn blend_mode(code: &[u8; 4]) -> Option<BlendMode> {
    use BlendMode::*;
    Some(match code {
        b"norm" => Normal,
        b"dark" => Darken,
        b"mult" => Multiply,
        b"cbrn" => ColorBurn,
        b"burn" => SoftBurn,
        b"lbrn" => LinearBurn,
        b"drkc" => DarkerColor,
        b"litn" => Lighten,
        b"scrn" => Screen,
        b"cddg" => ColorDodge,
        b"ddge" => SoftDodge,
        b"lddg" => Add,
        b"add " => Brighten,
        b"litc" => LighterColor,
        b"over" => Overlay,
        b"slit" => SoftLight,
        b"hlit" => HardLight,
        b"vlit" => VividLight,
        b"llit" => LinearLight,
        b"sbad" => BrightenDim,
        b"plit" => PinLight,
        b"hmix" => HardMix,
        b"diff" => Difference,
        b"excl" => Exclusion,
        b"fsub" => Subtract,
        b"sub " => Dim,
        b"fdiv" => Divide,
        b"hue " => Hue,
        b"sat " => Saturation,
        b"col " => Color,
        b"lum " => Luminosity,
        _ => return None,
    })
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
    let (w, h, chunks) = read_table(&mut file, size)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 测试用的写法，和读的代码反过来：位流从每个字节的最低位写起。
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        n: usize,
    }

    impl BitWriter {
        fn bit(&mut self, b: u32) {
            if self.n.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if b != 0 {
                *self.bytes.last_mut().unwrap() |= 1 << (self.n % 8);
            }
            self.n += 1;
        }

        fn bits(&mut self, v: u32, n: u32) {
            for i in 0..n {
                self.bit((v >> i) & 1);
            }
        }
    }

    /// 一个通道（`width` 宽）的值写成差的位流（实现方案 §3.21）。
    fn encode_channel(w: &mut BitWriter, values: &[i32], width: usize, max: i32) {
        let mut res = vec![0; values.len()];
        for k in 0..values.len() {
            let (x, y) = (k % width, k / width);
            let pred = if y == 0 {
                if x == 0 {
                    0
                } else {
                    values[k - 1]
                }
            } else if x == 0 {
                values[k - width]
            } else {
                (values[k - 1] + values[k - width] - values[k - width - 1]).clamp(0, max)
            };
            res[k] = values[k] - pred;
        }
        let mut k = 0;
        while k < res.len() {
            if res[k] == 0 {
                let run = res[k..].iter().take_while(|&&r| r == 0).count();
                if run >= 8 {
                    let n = run.min(134);
                    w.bits(0, 7);
                    w.bit(1);
                    w.bit(1);
                    w.bits((n - 8) as u32, 7);
                    k += n;
                } else {
                    w.bit(1);
                    w.bit(0);
                    k += 1;
                }
                continue;
            }
            let mag = res[k].unsigned_abs() + 1;
            let n = 32 - mag.leading_zeros();
            w.bits(0, (n - 1) / 2);
            w.bit(1);
            w.bit((n - 1) % 2);
            w.bits(mag - (1 << (n - 1)), n - 1);
            w.bit((res[k] < 0) as u32);
            k += 1;
        }
    }

    /// 一块压缩块：各通道 1024 个值。
    fn coded_block(channels: &[Vec<i32>]) -> Vec<u8> {
        let mut w = BitWriter::default();
        for c in channels {
            encode_channel(&mut w, c, BLOCK, FULL);
        }
        w.bytes
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

    fn dpcm(strips: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"dpcm".to_vec();
        strips.iter().for_each(|s| out.extend((s.len() as u32).to_le_bytes()));
        strips.iter().for_each(|s| out.extend(s));
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out
    }

    /// 一条图层记录。`area` 是（左, 上, 宽, 高），以 32 像素为单位。
    #[allow(clippy::too_many_arguments)]
    fn record(id: u32, kind: &[u8; 4], area: (i32, i32, u32, u32), blend: &[u8; 4], opacity: u32, flags: [u8; 4], name: &str, mask: Option<[u8; 24]>) -> Vec<u8> {
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
                (b"layr", 4, record(4, b"fold", (0, 0, 0, 0), b"pass", 100, [0, 0, 1, 0x40], "组", None)),
                (b"layr", 6, record(6, b"norm", (1, 0, 1, 1), b"norm", 100, [1, 0, 1, 0], "显示", Some(lmsk(10, 1, 0)))),
                (b"layr", 3, record(3, b"norm", (1, 0, 1, 1), b"mult", 50, [1, 0, 1, 0], "挡住", Some(lmsk(9, 1, 1)))),
                (b"layr", 2, record(2, b"norm", (0, 0, 3, 2), b"add ", 100, [0, 1, 1, 1], "Layer", None)),
                (b"layr", 5, record(5, b"symm", (0, 0, 0, 0), b"norm", 100, [0, 0, 1, 0], "尺子", None)),
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
        let one = |strip: Vec<u8>| file(64, 32, &[(b"layr", 2, record(2, b"norm", (0, 0, 1, 1), b"norm", 100, [0, 0, 1, 0], "x", None)), (b"lpix", 2, dpcm(&[strip]))]);
        // 块的标记里的列号不对。
        assert!(error(&one(vec![0xff, 0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xf1])).contains("块的标记"));
        // 压缩块的数据不够。
        assert!(error(&one(strip(0xff, 0, &[Piece::Coded(vec![0x80])]))).contains("压缩块"));
        // 尺寸太大。
        let mut big = file(64, 32, &[]);
        big[20..24].copy_from_slice(&(MAX_SIDE as u32 + 1).to_le_bytes());
        assert!(error(&big).contains("不支持"));
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
