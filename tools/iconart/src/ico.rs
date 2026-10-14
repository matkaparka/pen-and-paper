// SPDX-License-Identifier: GPL-3.0-or-later
//! PNG 编码与 .ico 文件写出。
//! .ico 里 64 像素及以上的条目存 PNG（Vista 起系统都认），48 像素及以下存传统的 32 位 DIB，
//! 这样老一点的工具和 API 读小图标时也不会出问题。

use std::fs;
use std::io;
use std::path::Path;

/// 非预乘 RGBA8 → PNG 字节
pub fn png_bytes(rgba: &[u8], w: usize, h: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Best);
        let mut wr = enc.write_header().map_err(io::Error::other)?;
        wr.write_image_data(rgba).map_err(io::Error::other)?;
        wr.finish().map_err(io::Error::other)?;
    }
    Ok(buf)
}

pub fn write_png(path: &Path, rgba: &[u8], w: usize, h: usize) -> io::Result<()> {
    fs::write(path, png_bytes(rgba, w, h)?)
}

/// 32 位 DIB 格式的图标条目：BITMAPINFOHEADER（高度写两倍）+ 自下而上的 BGRA + 1 位 AND 掩码
fn dib_bytes(rgba: &[u8], s: usize) -> Vec<u8> {
    let mask_stride = s.div_ceil(32) * 4;
    let xor_size = s * s * 4;
    let and_size = mask_stride * s;
    let mut b = Vec::with_capacity(40 + xor_size + and_size);
    let le32 = |b: &mut Vec<u8>, x: u32| b.extend_from_slice(&x.to_le_bytes());
    le32(&mut b, 40); // biSize
    le32(&mut b, s as u32); // biWidth
    le32(&mut b, (s * 2) as u32); // biHeight：XOR 图 + AND 掩码
    b.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    b.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    le32(&mut b, 0); // biCompression = BI_RGB
    le32(&mut b, (xor_size + and_size) as u32); // biSizeImage
    le32(&mut b, 0);
    le32(&mut b, 0);
    le32(&mut b, 0);
    le32(&mut b, 0);
    for y in (0..s).rev() {
        for x in 0..s {
            let i = (y * s + x) * 4;
            b.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    // AND 掩码：完全透明的像素置 1（只对不认 alpha 的老代码有意义）
    for y in (0..s).rev() {
        let mut row = vec![0u8; mask_stride];
        for x in 0..s {
            if rgba[(y * s + x) * 4 + 3] == 0 {
                row[x / 8] |= 0x80 >> (x % 8);
            }
        }
        b.extend_from_slice(&row);
    }
    b
}

/// images：(边长, 非预乘 RGBA8)，按给定顺序写进 .ico
pub fn write_ico(path: &Path, images: &[(usize, Vec<u8>)]) -> io::Result<()> {
    let mut datas = Vec::new();
    for (s, rgba) in images {
        datas.push(if *s >= 64 {
            png_bytes(rgba, *s, *s)?
        } else {
            dib_bytes(rgba, *s)
        });
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // idReserved
    out.extend_from_slice(&1u16.to_le_bytes()); // idType = 图标
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len();
    for ((s, _), d) in images.iter().zip(&datas) {
        let dim = if *s >= 256 { 0 } else { *s as u8 }; // 0 表示 256
        out.extend_from_slice(&[dim, dim, 0, 0]); // 宽、高、调色板数、保留
        out.extend_from_slice(&1u16.to_le_bytes()); // wPlanes
        out.extend_from_slice(&32u16.to_le_bytes()); // wBitCount
        out.extend_from_slice(&(d.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += d.len();
    }
    for d in &datas {
        out.extend_from_slice(d);
    }
    fs::write(path, out)
}
