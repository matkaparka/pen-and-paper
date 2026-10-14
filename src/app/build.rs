// SPDX-License-Identifier: GPL-3.0-or-later
//! 构建脚本：在 Windows + MSVC 工具链下，把 assets/icon.ico 和版本信息写成一个 .res 资源文件，
//! 交给链接器嵌进 zhibi.exe。不依赖 rc.exe：.res 的二进制格式按微软公开文档自己拼
//! （32 位资源文件：开头一个空资源项，每项头部和数据都按 4 字节对齐，小端）。
//! 其它平台或工具链什么也不做。
//!
//! 资源内容：
//! - RT_ICON：.ico 里的每个尺寸各一项，编号 1..N；
//! - RT_GROUP_ICON：编号 1，程序里用 LoadIconW(hinstance, MAKEINTRESOURCE(1)) 取图标；
//! - RT_VERSION：文件说明、产品名、原始文件名、版本号（取自 Cargo.toml 的 version）。
//!
//! 图标本身由 tools/iconart 生成（在 tools 目录下 `cargo run --release -p iconart`）。

use std::env;
use std::fs;
use std::path::PathBuf;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;

/// 资源项头部的 MemoryFlags（Win32 下系统会忽略，这里照 rc.exe 的习惯填）
const MOVEABLE: u16 = 0x0010;
const PURE: u16 = 0x0020;
const DISCARDABLE: u16 = 0x1000;

/// 语言无关（LANG_NEUTRAL, SUBLANG_NEUTRAL），任何系统语言下都能找到
const LANG_NEUTRAL: u16 = 0;

/// 程序加载图标用的资源编号
const ICON_GROUP_ID: u16 = 1;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || target_env != "msvc" {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("缺少 CARGO_MANIFEST_DIR"));
    let ico_path = manifest_dir.join("assets").join("icon.ico");
    let ico = fs::read(&ico_path).unwrap_or_else(|e| panic!("读取 {} 失败：{e}", ico_path.display()));
    let images = parse_ico(&ico);

    let mut res = Vec::new();
    // 开头的空资源项：表明这是 32 位资源文件
    push_resource(&mut res, 0, 0, 0, 0, &[]);

    // 每个尺寸一项 RT_ICON，同时拼出 GRPICONDIR
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes()); // idReserved
    group.extend_from_slice(&1u16.to_le_bytes()); // idType = 图标
    group.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, img) in images.iter().enumerate() {
        let id = (i + 1) as u16;
        push_resource(&mut res, RT_ICON, id, MOVEABLE | DISCARDABLE, LANG_NEUTRAL, img.data);
        // GRPICONDIRENTRY：和 .ico 的目录项一样，只是最后的文件偏移换成 2 字节的资源编号
        group.extend_from_slice(&img.entry[..12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    push_resource(
        &mut res,
        RT_GROUP_ICON,
        ICON_GROUP_ID,
        MOVEABLE | PURE | DISCARDABLE,
        LANG_NEUTRAL,
        &group,
    );

    push_resource(&mut res, RT_VERSION, 1, MOVEABLE | PURE, LANG_NEUTRAL, &version_info());

    let out = PathBuf::from(env::var("OUT_DIR").expect("缺少 OUT_DIR")).join("zhibi.res");
    fs::write(&out, &res).unwrap_or_else(|e| panic!("写 {} 失败：{e}", out.display()));
    // MSVC 的 link.exe 可以直接吃 .res 文件
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

struct IcoImage<'a> {
    /// .ico 里的 16 字节目录项（ICONDIRENTRY）
    entry: &'a [u8],
    /// 图像数据（PNG 或 DIB），原样放进 RT_ICON
    data: &'a [u8],
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// 解析 .ico：ICONDIR（6 字节）+ ICONDIRENTRY × N（各 16 字节）+ 图像数据
fn parse_ico(b: &[u8]) -> Vec<IcoImage<'_>> {
    assert!(
        b.len() >= 6 && le16(b, 0) == 0 && le16(b, 2) == 1,
        "assets/icon.ico 不是有效的 .ico 文件"
    );
    let n = le16(b, 4) as usize;
    assert!(n > 0 && b.len() >= 6 + 16 * n, "assets/icon.ico 目录不完整");
    (0..n)
        .map(|i| {
            let entry = &b[6 + 16 * i..6 + 16 * (i + 1)];
            let size = le32(entry, 8) as usize;
            let offset = le32(entry, 12) as usize;
            assert!(offset + size <= b.len(), "assets/icon.ico 第 {i} 项数据越界");
            IcoImage {
                entry,
                data: &b[offset..offset + size],
            }
        })
        .collect()
}

fn pad4(b: &mut Vec<u8>) {
    b.resize(b.len().next_multiple_of(4), 0);
}

/// 追加一个资源项：RESOURCEHEADER（类型和名字都用数字编号，头部固定 32 字节）+ 数据 + 对齐
fn push_resource(res: &mut Vec<u8>, ty: u16, name: u16, flags: u16, lang: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DataSize
    res.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    res.extend_from_slice(&0xffffu16.to_le_bytes()); // TYPE：0xFFFF 表示后面是数字编号
    res.extend_from_slice(&ty.to_le_bytes());
    res.extend_from_slice(&0xffffu16.to_le_bytes()); // NAME：同上
    res.extend_from_slice(&name.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    res.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    res.extend_from_slice(&lang.to_le_bytes()); // LanguageId
    res.extend_from_slice(&0u32.to_le_bytes()); // Version
    res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(data);
    pad4(res);
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(|u| u.to_le_bytes())
        .collect()
}

/// 版本资源里的一个块：wLength, wValueLength, wType, szKey, 对齐, Value, 对齐, Children…
/// value_len 按各结构的约定填（字符串按 WCHAR 个数，二进制按字节数）。
fn ver_block(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0u8; 6];
    b[2..4].copy_from_slice(&value_len.to_le_bytes());
    b[4..6].copy_from_slice(&u16::from(text).to_le_bytes()); // wType：1 文本，0 二进制
    b.extend_from_slice(&utf16z(key));
    pad4(&mut b);
    b.extend_from_slice(value);
    for c in children {
        pad4(&mut b);
        b.extend_from_slice(c);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b
}

fn ver_string(key: &str, value: &str) -> Vec<u8> {
    let v = utf16z(value);
    ver_block(key, &v, (v.len() / 2) as u16, true, &[])
}

/// VS_VERSIONINFO：VS_FIXEDFILEINFO + StringFileInfo（简体中文 0x0804，代码页 1200 = UTF-16）+ VarFileInfo
fn version_info() -> Vec<u8> {
    let part = |name: &str| env::var(name).ok().and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
    let (major, minor, patch) = (
        part("CARGO_PKG_VERSION_MAJOR"),
        part("CARGO_PKG_VERSION_MINOR"),
        part("CARGO_PKG_VERSION_PATCH"),
    );
    let ver_text = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| format!("{major}.{minor}.{patch}"));
    let ms = (u32::from(major) << 16) | u32::from(minor);
    let ls = u32::from(patch) << 16;

    let mut fixed = Vec::with_capacity(52);
    for x in [
        0xfeef_04bd, // dwSignature
        0x0001_0000, // dwStrucVersion
        ms,          // dwFileVersionMS
        ls,          // dwFileVersionLS
        ms,          // dwProductVersionMS
        ls,          // dwProductVersionLS
        0x3f,        // dwFileFlagsMask
        0,           // dwFileFlags
        0x0004_0004, // dwFileOS = VOS_NT_WINDOWS32
        1,           // dwFileType = VFT_APP
        0,           // dwFileSubtype
        0,           // dwFileDateMS
        0,           // dwFileDateLS
    ] {
        fixed.extend_from_slice(&u32::to_le_bytes(x));
    }

    let strings = [
        ver_string("FileDescription", "Pen & Paper 纸笔"),
        ver_string("FileVersion", &ver_text),
        ver_string("InternalName", "zhibi"),
        ver_string("OriginalFilename", "zhibi.exe"),
        ver_string("ProductName", "Pen & Paper"),
        ver_string("ProductVersion", &ver_text),
    ];
    let table = ver_block("080404b0", &[], 0, true, &strings);
    let string_file_info = ver_block("StringFileInfo", &[], 0, true, &[table]);

    let mut translation = Vec::new();
    translation.extend_from_slice(&0x0804u16.to_le_bytes()); // 语言：简体中文
    translation.extend_from_slice(&1200u16.to_le_bytes()); // 代码页：UTF-16
    let var = ver_block("Translation", &translation, 4, false, &[]);
    let var_file_info = ver_block("VarFileInfo", &[], 0, true, &[var]);

    ver_block("VS_VERSION_INFO", &fixed, 52, false, &[string_file_info, var_file_info])
}
