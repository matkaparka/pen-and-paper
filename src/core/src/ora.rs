// SPDX-License-Identifier: GPL-3.0-or-later
//! OpenRaster（.ora）的读写。
//!
//! 按公开的 OpenRaster 规范（<https://www.openraster.org/>，baseline 部分）实现。文件是一个 ZIP 压缩包：
//!
//! - `mimetype`：第一个条目，不压缩，内容是 `image/openraster`；
//! - `stack.xml`：画布的尺寸、分辨率和图层列表，最上面的图层排在最前；
//! - `data/*.png`：各图层的像素，8 位 RGBA、非预乘透明度，只含有内容的那块矩形；
//! - `mergedimage.png`：合成好的整张画布；
//! - `Thumbnails/thumbnail.png`：缩略图，长边不超过 256 像素。
//!
//! 这是和别的绘画软件交换用的格式，每通道只有 8 位，存一趟会丢掉本程序 15 位精度里多出来的那部分；
//! 要原样保存请用自己的格式（`format` 模块）。
//!
//! 文件夹（规格 F-055）写成嵌套的 `<stack>`：穿透的写 `isolation="auto"`，普通的写 `isolation="isolate"`
//! 和它的混合模式；读的时候反过来。规范里没有蒙版和填充图层：导出时把图层自己和外面各层文件夹的蒙版
//! 乘进像素，填充图层画成像素（`Document::baked_pixels`）。
//!
//! 规范里没有"画布背景色"。保存时把背景色写成最底下一个名叫"背景"（英文界面下叫"Background"）的
//! 不透明图层；读的时候，最底下的图层如果正是这个样子（名叫"背景"或"Background"、盖满画布、不透明、
//! 只有一种颜色），就还原成背景色。

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::Path;

use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use self::shared::{
    background_name, content_bounds, invalid, read_row, save_atomically, store_row, take_background, Notes, MAX_LAYERS,
};
use crate::document::{BlendMode, Document, Layer, LayerProps, MAX_SIDE};
use crate::lang::tr;
use crate::tile::TileMap;
use crate::trf;
use crate::Rect;

/// 文件扩展名（不含点）
pub const EXTENSION: &str = "ora";

const MIMETYPE: &str = "mimetype";
const MIMETYPE_CONTENT: &str = "image/openraster";
const STACK_XML: &str = "stack.xml";
const MERGED_PNG: &str = "mergedimage.png";
const THUMBNAIL_PNG: &str = "Thumbnails/thumbnail.png";
const BACKGROUND_PNG: &str = "data/background.png";
/// 写进 `stack.xml` 的规范版本：用到的东西（分辨率、这八种混合模式）在 0.0.3 里就都有了。
const SPEC_VERSION: &str = "0.0.3";
/// 缩略图长边的上限（像素），规范定的。
const THUMBNAIL_SIDE: usize = 256;
/// 文件里没写分辨率时的值，规范定的。
const DEFAULT_DPI: u32 = 72;
/// `stack.xml` 解压后的大小上限。正常只有几 KB，超出的只可能是坏文件或者压缩炸弹。
const MAX_XML_BYTES: u64 = 4 << 20;
/// `stack.xml` 里元素嵌套的层数和元素总数的上限，同样是为了挡住故意构造的文件。
const MAX_XML_DEPTH: usize = 64;
const MAX_XML_ELEMENTS: usize = 100_000;
/// 图层图像边长的上限。图层可以比画布大（多出来的部分读入时裁掉），但大到这个程度就不正常了。
const MAX_PNG_SIDE: u32 = 2 * MAX_SIDE as u32;
/// 隔行扫描的图像只能整幅解码，这是整幅缓冲区的上限。
const MAX_INTERLACED_BYTES: usize = 1 << 30;
/// 图层在画布上的偏移量夹到这个范围里，后面的加减法就不会溢出。
const MAX_OFFSET: i64 = 1 << 40;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// 混合模式在 `stack.xml` 里的名字（`composite-op` 属性）。
const COMPOSITE_OPS: [(BlendMode, &str); 8] = [
    (BlendMode::Normal, "svg:src-over"),
    (BlendMode::Multiply, "svg:multiply"),
    (BlendMode::Screen, "svg:screen"),
    (BlendMode::Overlay, "svg:overlay"),
    (BlendMode::Darken, "svg:darken"),
    (BlendMode::Lighten, "svg:lighten"),
    (BlendMode::Add, "svg:plus"),
    (BlendMode::Difference, "svg:difference"),
];

fn composite_op(mode: BlendMode) -> &'static str {
    COMPOSITE_OPS.iter().find(|(m, _)| *m == mode).map_or("svg:src-over", |(_, op)| op)
}

fn blend_of(op: &str) -> Option<BlendMode> {
    COMPOSITE_OPS.iter().find(|(_, o)| *o == op).map(|(m, _)| *m)
}

// ---- 保存 ----

/// 把文档存成 OpenRaster 文件。
///
/// 和 `format::save` 一样，先写进同一目录下的临时文件，完整落盘之后再改名顶替 `path`。
/// 不会把文档标成"已保存"：这种格式存不下文档的全部内容（见模块说明），算不算保存过由调用方决定。
pub fn save(doc: &Document, path: &Path) -> io::Result<()> {
    shared::check_document(doc)?;
    save_atomically(path, |file| write_archive(doc, file))
}

fn write_archive(doc: &Document, file: File) -> io::Result<()> {
    let keep = doc.keep_bounds();
    let layers = doc.layers();
    // 蒙版、填充图层都乘进（画进）像素；文件夹没有像素。伸出画布的内容照写（位置可以是负的）。
    let pixels: Vec<TileMap> = (0..layers.len()).map(|i| if layers[i].is_folder() { TileMap::default() } else { doc.baked_pixels(i) }).collect();
    let rects: Vec<Rect> = pixels.iter().map(|t| content_bounds(t, keep)).collect();
    let file_name = |i: usize| format!("data/layer{i:03}.png");

    let dpi = doc.dpi.max(1);
    let mut xml = String::from("<?xml version='1.0' encoding='UTF-8'?>\n");
    let _ = writeln!(xml, r#"<image version="{SPEC_VERSION}" w="{}" h="{}" xres="{dpi}" yres="{dpi}">"#, doc.width, doc.height);
    xml.push_str(" <stack>\n");
    // 规范规定最上面的图层写在最前面；文件夹的 <stack> 写在它里面的东西前面，往下一行浅回去时关上。
    let mut open = 0usize;
    for (i, (layer, r)) in layers.iter().zip(&rects).enumerate().rev() {
        while open > layer.depth as usize {
            open -= 1;
            let _ = writeln!(xml, "{} </stack>", "  ".repeat(open + 1));
        }
        let indent = "  ".repeat(open + 1);
        if layer.is_folder() {
            push_stack_element(&mut xml, &indent, &layer.props);
            open += 1;
        } else {
            xml.push_str(&indent);
            push_layer_element(&mut xml, &layer.props, &file_name(i), (r.x0, r.y0));
        }
    }
    while open > 0 {
        open -= 1;
        let _ = writeln!(xml, "{} </stack>", "  ".repeat(open + 1));
    }
    let background = LayerProps { name: background_name().into(), ..Default::default() };
    xml.push_str("  ");
    push_layer_element(&mut xml, &background, BACKGROUND_PNG, (0, 0));
    xml.push_str(" </stack>\n</image>\n");

    // 所有条目都原样存放：`mimetype` 是规范要求的，PNG 自己已经压过了，stack.xml 很小。
    // 每个条目都先在内存里编码好再整块写进去，道理和 `format` 模块里一样。
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let mut add = |name: &str, data: &[u8]| -> io::Result<()> {
        zip.start_file(name, stored)?;
        zip.write_all(data)
    };
    add(MIMETYPE, MIMETYPE_CONTENT.as_bytes())?;
    add(STACK_XML, xml.as_bytes())?;
    for (i, (tiles, r)) in pixels.iter().zip(&rects).enumerate() {
        if layers[i].is_folder() {
            continue;
        }
        let png = if r.is_empty() {
            // 规范要求每个 <layer> 都有图像，空图层就给一张 1×1 的全透明图。
            encode_rgba8(1, 1, |_, row| row.fill(0))?
        } else {
            let (w, h) = ((r.x1 - r.x0) as u32, (r.y1 - r.y0) as u32);
            encode_rgba8(w, h, |y, row| read_row(tiles, r.y0 + y as i32, r.x0, r.x1, row))?
        };
        add(&file_name(i), &png)?;
    }
    let (w, h) = (doc.width as usize, doc.height as usize);
    let [r, g, b] = doc.background;
    add(BACKGROUND_PNG, &encode_rgba8(w as u32, h as u32, |_, row| row.as_chunks_mut::<4>().0.fill([r, g, b, 255]))?)?;
    let rgb = doc.flatten_rgb();
    add(MERGED_PNG, &encode_rgb8(w, h, &rgb)?)?;
    let (thumb, tw, th) = thumbnail(&rgb, w, h);
    drop(rgb);
    add(THUMBNAIL_PNG, &encode_rgb8(tw, th, &thumb)?)?;

    let file = zip.finish()?.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

/// 往 `stack.xml` 里加一个文件夹的 `<stack>` 开头（实现方案 §3.11）：穿透的是 `isolation="auto"`、
/// 混合模式是普通的覆盖；普通的文件夹是 `isolation="isolate"` 和它自己的混合模式。
fn push_stack_element(xml: &mut String, indent: &str, props: &LayerProps) {
    let opacity = if props.opacity.is_nan() { 0.0 } else { props.opacity.clamp(0.0, 1.0) };
    let (isolation, op) = if props.pass_through { ("auto", composite_op(BlendMode::Normal)) } else { ("isolate", composite_op(props.blend)) };
    let _ = writeln!(
        xml,
        r#"{indent}<stack name="{}" opacity="{opacity}" visibility="{}" composite-op="{op}" isolation="{isolation}">"#,
        escape_attr(&props.name),
        if props.visible { "visible" } else { "hidden" },
    );
}

/// 往 `stack.xml` 里加一个 `<layer>`。`offset` 是图像左上角在画布上的位置。
fn push_layer_element(xml: &mut String, props: &LayerProps, src: &str, offset: (i32, i32)) {
    // 合成时非数当 0 用，这里保持一致。f32 的 Display 给出的是能原样读回来的最短写法（0.3 而不是 0.30000001）。
    let opacity = if props.opacity.is_nan() { 0.0 } else { props.opacity.clamp(0.0, 1.0) };
    let _ = writeln!(
        xml,
        r#"<layer name="{}" src="{}" x="{}" y="{}" opacity="{opacity}" visibility="{}" composite-op="{}"/>"#,
        escape_attr(&props.name),
        escape_attr(src),
        offset.0,
        offset.1,
        if props.visible { "visible" } else { "hidden" },
        composite_op(props.blend),
    );
}

/// 把一段文字写成 XML 属性值（不含两边的引号）。
fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // 属性值里直接写的换行和制表符，读的时候会被当成空格；写成字符引用才能原样回来。
            '\t' | '\n' | '\r' => {
                let _ = write!(out, "&#{};", c as u32);
            }
            // 其余的控制字符和这两个码位在 XML 1.0 里根本不许出现。
            c if (c as u32) < 0x20 || matches!(c, '\u{FFFE}' | '\u{FFFF}') => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// 8 位 RGBA 的 PNG（图层图像）。`fill_row(y, row)` 逐行给出像素，不用整幅的缓冲区。
fn encode_rgba8(width: u32, height: u32, mut fill_row: impl FnMut(u32, &mut [u8])) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    // 保存要快：大画布上一个图层就有上百 MB 的原始数据，所以用最快的压缩级别。
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header()?;
    let mut stream = writer.stream_writer()?;
    let mut row = vec![0u8; width as usize * 4];
    for y in 0..height {
        fill_row(y, &mut row);
        stream.write_all(&row)?;
    }
    stream.finish()?;
    writer.finish()?;
    Ok(out)
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
/// 本来就不超过的原样返回。返回像素和新的宽、高。（和 `format` 模块里的是同一个做法。）
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

/// 读 OpenRaster 文件。返回文档，以及读入时被忽略或近似处理的内容的说明（给用户看的句子，按当前的界面语言写，
/// 没有则为空）。
/// 读出的文档没有撤销记录；最上面的图层是当前图层。
///
/// 文件打不开时原样返回系统的错误；文件内容不对时返回 `InvalidData`，附带说明。任何输入都不会让它崩溃。
///
/// 本程序没有的东西这样处理：
/// - 图层组读成文件夹，`isolation="auto"` 的是穿透的，别的是普通文件夹；
/// - 不认识的混合模式改成"正常"；
/// - 图层超出画布的部分裁掉；
/// - 不是 PNG 的图层图像（规范允许别的格式，但不属于 baseline）跳过；
/// - 没有"背景"图层的文件，背景色是白色。
pub fn load(path: &Path) -> io::Result<(Document, Vec<String>)> {
    let file = File::open(path)?;
    let mut zip = ZipArchive::new(BufReader::new(file)).map_err(|e| {
        invalid(trf!("不是 OpenRaster 文件（不是有效的 ZIP 压缩包）：{e}", "Not an OpenRaster file (not a valid ZIP archive): {e}"))
    })?;
    let mut notes = Notes::default();
    let stack = parse_stack(&read_stack_xml(&mut zip)?, &mut notes)?;
    let canvas = Rect { x0: 0, y0: 0, x1: stack.width, y1: stack.height };
    let keep = Document::keep_area(stack.width, stack.height);

    let mut layers = Vec::with_capacity(stack.layers.len());
    // stack.xml 里最上面的图层排在最前，文档里的顺序是自下而上。
    for entry in stack.layers.into_iter().rev() {
        if entry.folder {
            if let Some(op) = &entry.unknown_op {
                let name = &entry.props.name;
                notes.layer(shared::unknown_blend_kind(), &trf!("{name}（{op:.40}）", "{name} ({op:.40})"));
            }
            let mut folder = Layer::folder(&entry.props.name);
            folder.props = entry.props;
            folder.depth = entry.depth;
            layers.push(folder);
            continue;
        }
        let mut tiles = TileMap::default();
        let (name, src) = (&entry.props.name, &entry.src);
        let outcome = read_layer_png(&mut zip, src, (entry.x, entry.y), keep, &mut tiles).map_err(|e| {
            io::Error::new(e.kind(), trf!("图层“{name:.80}”的图像 {src:.80}：{e}", "Image {src:.80} of layer \"{name:.80}\": {e}"))
        })?;
        match outcome {
            PngOutcome::NotPng => {
                notes.layer(tr("下列图层的图像不是 PNG，已跳过", "Skipped layers whose image is not a PNG"), name);
                continue;
            }
            PngOutcome::Read { clipped: true } => notes.layer(shared::clipped_kind(), name),
            PngOutcome::Read { clipped: false } => {}
        }
        if let Some(op) = &entry.unknown_op {
            notes.layer(shared::unknown_blend_kind(), &trf!("{name}（{op:.40}）", "{name} ({op:.40})"));
        }
        let mut layer = Layer::pixels(entry.props, tiles);
        layer.depth = entry.depth;
        layers.push(layer);
    }
    let background = take_background(&mut layers, canvas).unwrap_or([255; 3]);
    let mut doc = Document::from_layers(stack.width, stack.height, background, layers);
    doc.dpi = stack.dpi;
    Ok((doc, notes.into_vec()))
}

fn read_stack_xml<R: Read + Seek>(zip: &mut ZipArchive<R>) -> io::Result<String> {
    let entry = match zip.by_name(STACK_XML) {
        Ok(entry) => entry,
        Err(ZipError::FileNotFound) => {
            return Err(invalid(tr(
                "不是 OpenRaster 文件（压缩包里没有 stack.xml）",
                "Not an OpenRaster file (the archive has no stack.xml)",
            )))
        }
        Err(e) => return Err(invalid(trf!("无法读取 stack.xml：{e}", "Could not read stack.xml: {e}"))),
    };
    // 压缩包里登记的大小不可信，按实际解出来的字节数设限。
    let mut bytes = Vec::new();
    entry
        .take(MAX_XML_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| invalid(trf!("无法读取 stack.xml：{e}", "Could not read stack.xml: {e}")))?;
    if bytes.len() as u64 > MAX_XML_BYTES {
        return Err(invalid(tr("stack.xml 大得不正常，文件可能已损坏", "stack.xml is unusually large; the file may be damaged")));
    }
    String::from_utf8(bytes).map_err(|_| invalid(tr("stack.xml 不是 UTF-8 编码的文本", "stack.xml is not UTF-8 text")))
}

/// `stack.xml` 的内容，已经检查过。
struct Stack {
    width: i32,
    height: i32,
    dpi: u32,
    /// 自上而下，和文件里的顺序一样
    layers: Vec<StackLayer>,
}

struct StackLayer {
    props: LayerProps,
    /// 是文件夹（嵌套的 `<stack>`）
    folder: bool,
    /// 嵌套深度：0 是最外层
    depth: u16,
    /// 图像在压缩包里的路径（文件夹没有）
    src: String,
    /// 图像左上角在画布上的位置，可以是负的，也可以在画布之外
    x: i64,
    y: i64,
    /// 文件里写的混合模式，如果它是本程序没有的
    unknown_op: Option<String>,
}

fn parse_stack(text: &str, notes: &mut Notes) -> io::Result<Stack> {
    let root = parse_xml(text).map_err(|e| invalid(trf!("stack.xml 不是合法的 XML：{e}", "stack.xml is not valid XML: {e}")))?;
    if root.name != "image" {
        let root_name = &root.name;
        return Err(invalid(trf!(
            "不是 OpenRaster 文件（stack.xml 的根元素是 <{root_name:.40}>，应为 <image>）",
            "Not an OpenRaster file (the root element of stack.xml is <{root_name:.40}>, not <image>)"
        )));
    }
    let side = |key: &str| root.attr(key).and_then(|v| v.trim().parse::<i64>().ok());
    let (Some(width), Some(height)) = (side("w"), side("h")) else {
        return Err(invalid(tr(
            "stack.xml 里的画布尺寸 w、h 缺失或不是整数",
            "The canvas size (w, h) in stack.xml is missing or not an integer",
        )));
    };
    let sides = 1..=MAX_SIDE as i64;
    if !sides.contains(&width) || !sides.contains(&height) {
        return Err(invalid(trf!(
            "画布尺寸无效：{width}×{height}（每边应在 1 到 {MAX_SIDE} 之间）",
            "Invalid canvas size: {width}×{height} (each side must be 1 to {MAX_SIDE})"
        )));
    }

    // 分辨率只是附带的信息，缺失或者不对就用规范定的默认值。规范要求是整数，写成小数的也认。
    let resolution = |key: &str| {
        let v = root.attr(key)?.trim().parse::<f64>().ok()?;
        (v.is_finite() && v >= 0.5).then(|| v.round().min(u32::MAX as f64) as u32)
    };
    let (xres, yres) = (resolution("xres"), resolution("yres"));
    if let (Some(x), Some(y)) = (xres, yres) {
        if x != y {
            notes.add(trf!(
                "文件里横向和纵向的分辨率不一样（{x} 和 {y}），只保留了横向的",
                "The file has different horizontal and vertical resolutions ({x} and {y} dpi); kept the horizontal one"
            ));
        }
    }

    let stack = root
        .children
        .iter()
        .find(|c| c.name == "stack")
        .ok_or_else(|| invalid(tr("stack.xml 里没有图层列表 <stack>", "stack.xml has no layer list (<stack>)")))?;
    let mut flat = Flattened::default();
    flatten(stack, 0, &mut flat)?;
    if !flat.unknown.is_empty() {
        let names: Vec<String> = flat.unknown.iter().map(|n| format!("<{n:.40}>")).collect();
        let list = names.join(tr("、", ", "));
        notes.add(trf!("忽略了 stack.xml 里不认识的内容：{list}", "Ignored unknown content in stack.xml: {list}"));
    }
    // 没有名字的图层按它在图层（不算文件夹）里自下而上的位置起个名字；没有名字的文件夹（组）就叫"组"。
    let mut n = flat.layers.iter().filter(|l| !l.folder).count();
    for layer in &mut flat.layers {
        if layer.folder {
            if layer.props.name.is_empty() {
                layer.props.name = tr("组", "Group").to_string();
            }
            continue;
        }
        if layer.props.name.is_empty() {
            layer.props.name = trf!("图层 {n}", "Layer {n}");
        }
        n -= 1;
    }
    Ok(Stack { width: width as i32, height: height as i32, dpi: xres.or(yres).unwrap_or(DEFAULT_DPI), layers: flat.layers })
}

#[derive(Default)]
struct Flattened {
    /// 自上而下，文件夹排在它里面的东西前面
    layers: Vec<StackLayer>,
    /// 遇到的不认识的元素的名字
    unknown: Vec<String>,
}

/// 把一个 `<stack>` 里的东西按文件里的顺序（自上而下）接到 `out` 后面；嵌套的 `<stack>` 是文件夹，
/// 先记它自己，再记它里面的东西，深度加一。规范里 `isolation` 默认是 `isolate`，就是普通文件夹；
/// `auto` 是穿透的，这时规范说文件夹的混合模式不起作用，就不管它。
/// 嵌套的层数不会超过 `MAX_XML_DEPTH`（解析 XML 时已经限制），所以可以放心地递归。
fn flatten(stack: &Element, depth: u16, out: &mut Flattened) -> io::Result<()> {
    for child in &stack.children {
        match child.name.as_str() {
            "stack" => {
                if out.layers.len() >= MAX_LAYERS {
                    return Err(invalid(trf!(
                        "图层太多（超过 {MAX_LAYERS} 个），文件可能已损坏",
                        "Too many layers (more than {MAX_LAYERS}); the file may be damaged"
                    )));
                }
                let (visible, opacity) = visibility_and_opacity(child, tr("图层组", "a layer group"))?;
                let pass_through = child.attr("isolation").map(str::trim) == Some("auto");
                let op = child.attr("composite-op").map_or("svg:src-over", str::trim);
                let blend = if pass_through { Some(BlendMode::Normal) } else { blend_of(op) };
                out.layers.push(StackLayer {
                    props: LayerProps {
                        name: child.attr("name").unwrap_or_default().to_string(),
                        opacity,
                        visible,
                        blend: blend.unwrap_or(BlendMode::Normal),
                        pass_through,
                        ..Default::default()
                    },
                    folder: true,
                    depth,
                    src: String::new(),
                    x: 0,
                    y: 0,
                    unknown_op: blend.is_none().then(|| op.to_string()),
                });
                flatten(child, depth.saturating_add(1), out)?;
            }
            "layer" => {
                if out.layers.len() >= MAX_LAYERS {
                    return Err(invalid(trf!(
                        "图层太多（超过 {MAX_LAYERS} 个），文件可能已损坏",
                        "Too many layers (more than {MAX_LAYERS}); the file may be damaged"
                    )));
                }
                let index = out.layers.len() + 1;
                let bad =
                    |what: &str| invalid(trf!("stack.xml 里自上而下第 {index} 个图层{what}", "Layer {index} from the top in stack.xml {what}"));
                let src = child
                    .attr("src")
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| bad(tr("没有指定图像（src）", "has no image (src)")))?;
                let offset = |key: &str| match child.attr(key).map(str::trim) {
                    None | Some("") => Ok(0),
                    Some(v) => parse_offset(v).ok_or_else(|| {
                        bad(&trf!("的位置 {key} 不是数字：{v:.40}", "has a position {key} that is not a number: {v:.40}"))
                    }),
                };
                let (layer_visible, layer_opacity) =
                    visibility_and_opacity(child, &trf!("自上而下第 {index} 个图层", "layer {index} from the top"))?;
                let op = child.attr("composite-op").map_or("svg:src-over", str::trim);
                let blend = blend_of(op);
                out.layers.push(StackLayer {
                    props: LayerProps {
                        name: child.attr("name").unwrap_or_default().to_string(),
                        opacity: layer_opacity,
                        visible: layer_visible,
                        blend: blend.unwrap_or(BlendMode::Normal),
                        ..Default::default()
                    },
                    folder: false,
                    depth,
                    src: src.to_string(),
                    x: offset("x")?,
                    y: offset("y")?,
                    unknown_op: blend.is_none().then(|| op.to_string()),
                });
            }
            other => {
                if !out.unknown.iter().any(|n| n == other) && out.unknown.len() < 8 {
                    out.unknown.push(other.to_string());
                }
            }
        }
    }
    Ok(())
}

/// 图层和图层组共有的两个属性：`visibility`（默认可见）和 `opacity`（默认 1）。
/// `what` 是出错时用来指明是哪个元素的话，已经按当前语言写好。
fn visibility_and_opacity(el: &Element, what: &str) -> io::Result<(bool, f32)> {
    let opacity = match el.attr("opacity").map(str::trim) {
        None | Some("") => 1.0,
        Some(v) => match v.parse::<f32>() {
            Ok(o) if !o.is_nan() => o.clamp(0.0, 1.0),
            _ => {
                return Err(invalid(trf!(
                    "stack.xml 里{what}的不透明度不是数字：{v:.40}",
                    "The opacity of {what} in stack.xml is not a number: {v:.40}"
                )))
            }
        },
    };
    Ok((el.attr("visibility").map(str::trim) != Some("hidden"), opacity))
}

/// 图层的偏移量。规范要求是整数；写成小数的四舍五入。
fn parse_offset(v: &str) -> Option<i64> {
    let n = match v.parse::<i64>() {
        Ok(n) => n,
        Err(_) => {
            let f = v.parse::<f64>().ok().filter(|f| f.is_finite())?;
            f.round() as i64
        }
    };
    Some(n.clamp(-MAX_OFFSET, MAX_OFFSET))
}

enum PngOutcome {
    /// 压缩包里有这个文件，但它不是 PNG
    NotPng,
    /// 读进来了。`clipped` 表示有看得见的像素落在画布之外，被裁掉了
    Read { clipped: bool },
}

/// 把压缩包里的一张图层图像解码进 `tiles`。`offset` 是它的左上角在画布上的位置，伸出 `keep`
/// （图层能放到的范围）的部分裁掉。8 位和 16 位的灰度、灰度加透明度、RGB、RGBA 以及调色板图像都能读。
fn read_layer_png<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    src: &str,
    offset: (i64, i64),
    keep: Rect,
    tiles: &mut TileMap,
) -> io::Result<PngOutcome> {
    let mut entry = match zip.by_name(src) {
        Ok(entry) => entry,
        Err(ZipError::FileNotFound) => return Err(invalid(tr("不在压缩包里", "not in the archive"))),
        Err(e) => return Err(invalid(trf!("无法读取：{e}", "could not read: {e}"))),
    };
    // 先自己读文件头：8 个字节的签名，紧跟着是 IHDR 数据块，图像的宽和高在固定的位置上。
    // 知道了尺寸才好给后面的读取量设上限，免得一个很小的压缩包解出没完没了的数据。
    let mut head = [0u8; 33];
    let mut got = 0;
    while got < head.len() {
        match entry.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(invalid(trf!("无法读取：{e}", "could not read: {e}"))),
        }
    }
    let head = &head[..got];
    if !head.starts_with(PNG_SIGNATURE) {
        return Ok(PngOutcome::NotPng);
    }
    let field = |at: usize| head.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(width), Some(height)) = (field(16), field(20)) else {
        return Err(invalid(tr("无法解码：文件头不完整", "could not decode: incomplete header")));
    };
    if head[12..16] != *b"IHDR" {
        return Err(invalid(tr("无法解码：文件头不对", "could not decode: bad header")));
    }
    if !(1..=MAX_PNG_SIDE).contains(&width) || !(1..=MAX_PNG_SIDE).contains(&height) {
        return Err(invalid(trf!("尺寸不正常（{width}×{height}）", "unusual size ({width}×{height})")));
    }
    // 展开之后每个像素最多 8 个字节；正常的图像不会比没压缩的像素大出多少。
    let limit = 16 * width as u64 * height as u64 + (16 << 20);
    let mut decoder = png::Decoder::new(Cursor::new(head.to_vec()).chain(entry.take(limit)));
    // 调色板展开成 RGB，不足 8 位的灰度展开成 8 位，tRNS 透明色变成透明度通道。
    decoder.set_transformations(png::Transformations::EXPAND);
    let decode_error = |e: png::DecodingError| invalid(trf!("无法解码：{e}", "could not decode: {e}"));
    let mut reader = decoder.read_info().map_err(decode_error)?;
    let interlaced = reader.info().interlaced;
    if (reader.info().width, reader.info().height) != (width, height) {
        return Err(invalid(tr("无法解码：文件头不对", "could not decode: bad header")));
    }
    let (color, depth) = reader.output_color_type();
    let samples = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err(invalid(tr("不支持的像素格式", "unsupported pixel format"))),
    };
    let sample_bytes = match depth {
        png::BitDepth::Eight => 1,
        png::BitDepth::Sixteen => 2,
        _ => return Err(invalid(tr("不支持的像素格式", "unsupported pixel format"))),
    };
    let stride = width as usize * samples * sample_bytes;

    let (x0, y0) = offset;
    let mut clipped = false;
    let mut pixels: Vec<[u16; 4]> = Vec::with_capacity(width as usize);
    let mut put_row = |index: usize, data: &[u8]| {
        widen_row(data, samples, sample_bytes, &mut pixels);
        let y = y0 + index as i64;
        clipped |= if (keep.y0 as i64..keep.y1 as i64).contains(&y) {
            store_row(tiles, keep, x0, y as i32, &pixels)
        } else {
            pixels.iter().any(|p| p[3] != 0)
        };
    };

    if interlaced {
        // 隔行扫描的图只能整幅解码。
        let size = reader.output_buffer_size();
        if Some(size) != stride.checked_mul(height as usize) {
            return Err(invalid(tr("图像数据与文件头不符", "image data does not match its header")));
        }
        if size > MAX_INTERLACED_BYTES {
            return Err(invalid(tr("隔行扫描的图像太大", "interlaced image is too large")));
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(size)
            .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, tr("内存不足", "out of memory")))?;
        buf.resize(size, 0u8);
        let frame = reader.next_frame(&mut buf).map_err(decode_error)?;
        if (frame.width, frame.height, frame.line_size) != (width, height, stride) {
            return Err(invalid(tr("图像数据与文件头不符", "image data does not match its header")));
        }
        for (index, row) in buf.chunks_exact(stride).enumerate() {
            put_row(index, row);
        }
    } else {
        for index in 0..height as usize {
            let row = reader
                .next_row()
                .map_err(decode_error)?
                .ok_or_else(|| invalid(tr("图像数据不完整", "image data is incomplete")))?;
            if row.data().len() != stride {
                return Err(invalid(tr("图像数据与文件头不符", "image data does not match its header")));
            }
            put_row(index, row.data());
        }
        // 再要一行，让解码器把剩下的图像数据读完：数据块的校验和在块的末尾，读到那里才会核对。
        reader.next_row().map_err(decode_error)?;
    }
    Ok(PngOutcome::Read { clipped })
}

/// 把解码出来的一行换成 16 位、非预乘的 RGBA。每个像素有 `samples` 个采样（1 灰度、2 灰度加透明度、
/// 3 RGB、4 RGBA），每个采样 `sample_bytes` 个字节（大端）。8 位的值乘以 257 正好铺满 16 位。
fn widen_row(data: &[u8], samples: usize, sample_bytes: usize, out: &mut Vec<[u16; 4]>) {
    let sample = |px: &[u8], i: usize| {
        if sample_bytes == 2 {
            u16::from_be_bytes([px[i * 2], px[i * 2 + 1]])
        } else {
            px[i] as u16 * 257
        }
    };
    out.clear();
    out.extend(data.chunks_exact(samples * sample_bytes).map(|px| match samples {
        1 => [sample(px, 0), sample(px, 0), sample(px, 0), u16::MAX],
        2 => [sample(px, 0), sample(px, 0), sample(px, 0), sample(px, 1)],
        3 => [sample(px, 0), sample(px, 1), sample(px, 2), u16::MAX],
        _ => [sample(px, 0), sample(px, 1), sample(px, 2), sample(px, 3)],
    }));
}

// ---- XML ----
//
// stack.xml 只用到 XML 里很小的一部分，所以自己写了一个小解析器，不为它多引一个库。
// 支持：元素、属性、注释、处理指令、CDATA、五个预定义的实体和数字字符引用。
// 不支持 DTD 里自定义的实体（遇到就报错），顺带也就没有"实体炸弹"的问题。

/// 一个元素。只留用得上的：名字、属性、子元素；文字内容不要。
struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Element>,
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// 解析 XML，返回根元素。出错时返回给用户看的说明（按当前语言）。
fn parse_xml(text: &str) -> Result<Element, String> {
    // 有的编辑器和脚本会在开头加上 UTF-8 的 BOM。
    let mut p = XmlParser { rest: text.strip_prefix('\u{FEFF}').unwrap_or(text) };
    p.skip_misc(true)?;
    if !p.rest.starts_with('<') {
        return Err(tr("没有根元素", "no root element").into());
    }
    // 还没等到结束标签的元素，从外到内。用显式的栈而不是递归，嵌套多深都不会把调用栈撑爆。
    let mut open: Vec<Element> = Vec::new();
    let mut count = 0;
    let root = loop {
        // 文字内容用不上，直接跳到下一个标签。
        let at = p.rest.find('<').ok_or(tr("文件在根元素结束之前就完了", "the file ends before the root element is closed"))?;
        p.rest = &p.rest[at..];
        let finished = if p.eat("<!--") {
            p.skip_past("-->", tr("注释", "comment"))?;
            None
        } else if p.eat("<![CDATA[") {
            p.skip_past("]]>", tr("CDATA 段", "CDATA section"))?;
            None
        } else if p.eat("<?") {
            p.skip_past("?>", tr("处理指令", "processing instruction"))?;
            None
        } else if p.eat("</") {
            let name = p.name()?;
            p.skip_ws();
            if !p.eat(">") {
                return Err(trf!("结束标签 </{name:.40}> 没有写完", "end tag </{name:.40}> is incomplete"));
            }
            match open.pop() {
                Some(el) if el.name == name => Some(el),
                Some(el) => {
                    let start = &el.name;
                    return Err(trf!(
                        "结束标签 </{name:.40}> 和开始标签 <{start:.40}> 对不上",
                        "end tag </{name:.40}> does not match start tag <{start:.40}>"
                    ));
                }
                None => return Err(trf!("多出一个结束标签 </{name:.40}>", "extra end tag </{name:.40}>")),
            }
        } else {
            p.eat("<");
            count += 1;
            if count > MAX_XML_ELEMENTS {
                return Err(tr("元素太多", "too many elements").into());
            }
            let name = p.name()?.to_string();
            let (attrs, closed) = p.attributes()?;
            let el = Element { name, attrs, children: Vec::new() };
            if closed {
                Some(el)
            } else if open.len() >= MAX_XML_DEPTH {
                return Err(tr("元素嵌套得太深", "elements are nested too deeply").into());
            } else {
                open.push(el);
                None
            }
        };
        if let Some(el) = finished {
            match open.last_mut() {
                Some(parent) => parent.children.push(el),
                None => break el,
            }
        }
    };
    p.skip_misc(false)?;
    if !p.rest.is_empty() {
        return Err(tr("根元素之后还有别的内容", "extra content after the root element").into());
    }
    Ok(root)
}

struct XmlParser<'a> {
    /// 还没读的部分
    rest: &'a str,
}

impl<'a> XmlParser<'a> {
    /// 接下来如果正是 `literal` 就吃掉它。
    fn eat(&mut self, literal: &str) -> bool {
        match self.rest.strip_prefix(literal) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    fn skip_ws(&mut self) {
        self.rest = self.rest.trim_start_matches([' ', '\t', '\r', '\n']);
    }

    /// 跳到 `end` 之后。
    fn skip_past(&mut self, end: &str, what: &str) -> Result<(), String> {
        let at = self.rest.find(end).ok_or_else(|| trf!("{what}没有结束", "unclosed {what}"))?;
        self.rest = &self.rest[at + end.len()..];
        Ok(())
    }

    /// 跳过元素之外可以出现的东西：空白、注释、处理指令（XML 声明也算），`prolog` 为真时还有 DOCTYPE。
    fn skip_misc(&mut self, prolog: bool) -> Result<(), String> {
        loop {
            self.skip_ws();
            if self.eat("<!--") {
                self.skip_past("-->", tr("注释", "comment"))?;
            } else if self.eat("<?") {
                self.skip_past("?>", tr("处理指令", "processing instruction"))?;
            } else if prolog && self.eat("<!DOCTYPE") {
                self.skip_doctype()?;
            } else {
                return Ok(());
            }
        }
    }

    /// 跳过 DOCTYPE 声明余下的部分：到引号和方括号之外的第一个 `>` 为止。
    fn skip_doctype(&mut self) -> Result<(), String> {
        let (mut depth, mut quote) = (0usize, None);
        for (i, c) in self.rest.char_indices() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '"' | '\'') => quote = Some(c),
                (None, '[') => depth += 1,
                (None, ']') => depth = depth.saturating_sub(1),
                (None, '>') if depth == 0 => {
                    self.rest = &self.rest[i + 1..];
                    return Ok(());
                }
                _ => {}
            }
        }
        Err(tr("DOCTYPE 没有结束", "unclosed DOCTYPE").into())
    }

    /// 元素或属性的名字：到空白或者标签里有特殊含义的字符为止。
    fn name(&mut self) -> Result<&'a str, String> {
        let stop = |c: char| c.is_whitespace() || matches!(c, '/' | '>' | '=' | '<' | '"' | '\'');
        let end = self.rest.find(stop).unwrap_or(self.rest.len());
        if end == 0 {
            return Err(tr("标签或属性没有名字", "a tag or attribute has no name").into());
        }
        let (name, rest) = self.rest.split_at(end);
        self.rest = rest;
        Ok(name)
    }

    /// 开始标签里名字之后的部分：若干属性，直到 `>` 或 `/>`。返回属性，以及这个元素是不是就此结束（`/>`）。
    fn attributes(&mut self) -> Result<(Vec<(String, String)>, bool), String> {
        let mut attrs: Vec<(String, String)> = Vec::new();
        loop {
            self.skip_ws();
            if self.eat("/>") {
                return Ok((attrs, true));
            }
            if self.eat(">") {
                return Ok((attrs, false));
            }
            if self.rest.is_empty() {
                return Err(tr("标签没有结束", "unclosed tag").into());
            }
            let key = self.name()?;
            self.skip_ws();
            if !self.eat("=") {
                return Err(trf!("属性 {key:.40} 后面没有等号", "attribute {key:.40} has no equals sign"));
            }
            self.skip_ws();
            let quote = match self.rest.chars().next() {
                Some(q @ ('"' | '\'')) => q,
                _ => return Err(trf!("属性 {key:.40} 的值没有加引号", "the value of attribute {key:.40} is not quoted")),
            };
            let body = &self.rest[1..];
            let end = body.find(quote).ok_or_else(|| trf!("属性 {key:.40} 的值没有结束", "the value of attribute {key:.40} is not closed"))?;
            let raw = &body[..end];
            self.rest = &body[end + 1..];
            if raw.contains('<') {
                return Err(trf!("属性 {key:.40} 的值里有 <", "the value of attribute {key:.40} contains <"));
            }
            if attrs.iter().any(|(k, _)| k == key) {
                return Err(trf!("属性 {key:.40} 出现了两次", "attribute {key:.40} appears twice"));
            }
            attrs.push((key.to_string(), unescape(raw)?));
        }
    }
}

/// 把属性值里的实体和字符引用换回字符。
fn unescape(raw: &str) -> Result<String, String> {
    // 直接写在属性值里的换行和制表符，按 XML 的规定当作空格。
    let plain = |c: char| if matches!(c, '\t' | '\n' | '\r') { ' ' } else { c };
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.extend(rest[..at].chars().map(plain));
        rest = &rest[at + 1..];
        let end = rest.find(';').ok_or(tr("实体引用没有结束", "unclosed entity reference"))?;
        let entity = &rest[..end];
        rest = &rest[end + 1..];
        out.push(match entity {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = match (entity.strip_prefix("#x"), entity.strip_prefix('#')) {
                    (Some(hex), _) => u32::from_str_radix(hex, 16).ok(),
                    (None, Some(dec)) => dec.parse::<u32>().ok(),
                    (None, None) => None,
                };
                code.and_then(char::from_u32).ok_or_else(|| trf!("不认识的实体 &{entity:.20};", "unknown entity &{entity:.20};"))?
            }
        });
    }
    out.extend(rest.chars().map(plain));
    Ok(out)
}

/// 两种交换格式（OpenRaster 和 PSD）共用的部分：8 位非预乘像素与分块之间的换算、"背景"图层的约定、
/// 先写临时文件再改名的保存方式、给用户看的说明的汇总。`psd` 模块从这里引用。
pub(crate) mod shared {
    use std::ffi::OsString;
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::document::{BlendMode, Document, Layer, MAX_SIDE};
    use crate::lang::tr;
    use crate::tile::{tiles_in, TileMap, TILE};
    use crate::trf;
    use crate::{Rect, ONE};

    /// 代表画布背景色的那个图层的两种名字：中文、英文。
    const BACKGROUND_NAMES: [&str; 2] = ["背景", "Background"];

    /// 保存时代表画布背景色的那个图层的名字，按当前语言取。
    pub fn background_name() -> &'static str {
        tr(BACKGROUND_NAMES[0], BACKGROUND_NAMES[1])
    }

    /// 一个文件里图层个数的上限。正常的文件到不了这个数，超过的只可能是坏文件或者故意构造的。
    pub const MAX_LAYERS: usize = 10_000;
    /// 15 位定点里的 1.0，换算像素时用的宽类型。
    const ONE64: u64 = ONE as u64;
    /// 16 位通道里的 1.0。
    const MAX16: u64 = 65535;
    /// 8 位的透明度 1（即 1/255）读进来之后在分块里的值。
    const LOWEST_ALPHA: u64 = div_round(ONE64, 255);

    /// 文件内容有问题时的错误。
    pub fn invalid(msg: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, msg.into())
    }

    /// 保存之前检查文档：画布尺寸要在范围之内；图层（连同代表背景的那一个）不能多到自己都不肯读回来。
    pub fn check_document(doc: &Document) -> io::Result<()> {
        let bad = |msg: String| Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
        let (w, h) = (doc.width, doc.height);
        if !(1..=MAX_SIDE).contains(&w) || !(1..=MAX_SIDE).contains(&h) {
            return bad(trf!("画布尺寸无效：{w}×{h}", "Invalid canvas size: {w}×{h}"));
        }
        let (count, max) = (doc.layers().len(), MAX_LAYERS - 1);
        if count > max {
            return bad(trf!(
                "图层太多（{count} 个），这种格式最多存 {max} 个",
                "Too many layers ({count}); this format holds at most {max}"
            ));
        }
        Ok(())
    }

    // ---- 像素换算 ----
    //
    // 分块里是 15 位、预乘的，交换格式要 8 位、非预乘的。8 位装不下 15 位的全部信息，所以写出去是有损的；
    // 要保证的是另一个方向：本来就是 8 位的图像读进来再写出去，像素不变。
    //
    // 读：透明度 a15 = round(a8·32768/255)，颜色 c15 = round(c8·a15/255)。
    // 写：a8 = round(a15·255/32768)，c8 = round(c15·255/a15)。
    //
    // 读的时候颜色的舍入误差不超过半格，写回去要乘以 255/a15。只要 a15 > 255（也就是 a8 ≥ 2，那时 a15 ≥ 257），
    // 放大之后还是不到半格，四舍五入就回到原值。
    //
    // 剩下两种情况没法原样回来，都是预乘格式本身的限制：
    // - a8 = 0：全透明的像素在分块里没有颜色，写出去颜色一律是 0；
    // - a8 = 1：这时 a15 = 129，分块里的颜色只有 130 档，装不下 256 种 8 位颜色。写的时候把颜色归到
    //   "读回来再写出去不会变"的那 130 个值上（和原值最多差 1，在 1/255 的透明度下根本看不出来），
    //   这样存一次之后再怎么来回倒都不会接着变。

    /// 四舍五入的整数除法。
    #[inline]
    pub const fn div_round(n: u64, d: u64) -> u64 {
        (2 * n + d) / (2 * d)
    }

    /// 分块像素（15 位、预乘）→ 8 位、非预乘。
    #[inline]
    pub fn to_straight8(px: [u16; 4]) -> [u8; 4] {
        // 正常的像素满足 颜色 ≤ 透明度 ≤ ONE。不满足的在非预乘格式里没法表示，夹到最近的合法值。
        let a = (px[3] as u64).min(ONE64);
        let a8 = div_round(a * 255, ONE64);
        if a8 == 0 {
            return [0; 4];
        }
        let ch = |c: u16| {
            let c = (c as u64).min(a);
            // 不透明的像素最多，单独走移位，省掉一次真正的除法。
            let v = if a == ONE64 { (c * 255 + ONE64 / 2) >> 15 } else { div_round(c * 255, a) };
            if a8 == 1 {
                div_round(div_round(v * LOWEST_ALPHA, 255) * 255, LOWEST_ALPHA) as u8
            } else {
                v as u8
            }
        };
        [ch(px[0]), ch(px[1]), ch(px[2]), a8 as u8]
    }

    /// 16 位、非预乘 → 分块像素（15 位、预乘）。8 位的值先乘以 257 变成 16 位再来，结果和上面写的公式一样。
    #[inline]
    pub fn to_premultiplied(px: [u16; 4]) -> [u16; 4] {
        let a = div_round(px[3] as u64 * ONE64, MAX16);
        let ch = |c: u16| div_round(c as u64 * a, MAX16) as u16;
        [ch(px[0]), ch(px[1]), ch(px[2]), a as u16]
    }

    /// 图层在矩形 `area`（画布，或者图层能放到的范围）之内、按 8 位算不是全透明的像素的包围盒；
    /// 没有这样的像素就是空矩形。
    pub fn content_bounds(tiles: &TileMap, area: Rect) -> Rect {
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
                // 透明度低到写成 8 位是 0 的像素，写出去就是全透明的，不算内容。
                let seen = |x: i32| div_round(tile.0[(((y - oy) * TILE + (x - ox)) * 4 + 3) as usize] as u64 * 255, ONE64) != 0;
                let Some(first) = (clip.x0..clip.x1).find(|&x| seen(x)) else { continue };
                let last = (clip.x0..clip.x1).rev().find(|&x| seen(x)).unwrap_or(first);
                bounds = bounds.union(Rect { x0: first, y0: y, x1: last + 1, y1: y + 1 });
            }
        }
        bounds
    }

    /// 图层第 `y` 行上 `x0..x1` 这一段的像素（可以在画布外），写成 8 位、非预乘的 RGBA，放进 `out`。
    pub fn read_row(tiles: &TileMap, y: i32, x0: i32, x1: i32, out: &mut [u8]) {
        let ty = y.div_euclid(TILE);
        let mut x = x0;
        while x < x1 {
            let tx = x.div_euclid(TILE);
            let end = ((tx + 1) * TILE).min(x1);
            let dst = &mut out[((x - x0) * 4) as usize..((end - x0) * 4) as usize];
            match tiles.get((tx, ty)) {
                None => dst.fill(0),
                Some(tile) => {
                    let at = (((y - ty * TILE) * TILE + (x - tx * TILE)) * 4) as usize;
                    for (px, o) in tile.0[at..].as_chunks::<4>().0.iter().zip(dst.as_chunks_mut::<4>().0) {
                        *o = to_straight8(*px);
                    }
                }
            }
            x = end;
        }
    }

    /// 把一行 16 位、非预乘的 RGBA 写进分块。这一行在第 `y` 行上（要在 `area` 的上下范围之内），从横坐标
    /// `x_start` 开始往右；伸出 `area` 左右的部分裁掉。`area` 是图层能放到的范围（`Document::keep_area`），
    /// 读合并图像时是画布。整段全透明的地方不为它新建分块，图层照旧是稀疏的。
    ///
    /// 返回值：是不是有不透明的像素被裁掉了。
    pub fn store_row(tiles: &mut TileMap, area: Rect, x_start: i64, y: i32, row: &[[u16; 4]]) -> bool {
        let x_end = x_start.saturating_add(row.len() as i64);
        let (x0, x1) = (x_start.max(area.x0 as i64), x_end.min(area.x1 as i64));
        if x0 >= x1 {
            return row.iter().any(|p| p[3] != 0);
        }
        let (outside_left, rest) = row.split_at((x0 - x_start) as usize);
        let (inside, outside_right) = rest.split_at((x1 - x0) as usize);
        let (x0, x1) = (x0 as i32, x1 as i32);
        let ty = y.div_euclid(TILE);
        let mut x = x0;
        while x < x1 {
            let tx = x.div_euclid(TILE);
            let end = ((tx + 1) * TILE).min(x1);
            let src = &inside[(x - x0) as usize..(end - x0) as usize];
            if src.iter().any(|p| p[3] != 0) {
                let tile = tiles.get_mut((tx, ty));
                let at = (((y - ty * TILE) * TILE + (x - tx * TILE)) * 4) as usize;
                for (p, out) in src.iter().zip(tile.0[at..].as_chunks_mut::<4>().0) {
                    *out = to_premultiplied(*p);
                }
            }
            x = end;
        }
        outside_left.iter().chain(outside_right).any(|p| p[3] != 0)
    }

    // ---- 背景 ----

    /// 如果最底下的图层正是保存时代表背景色的那种图层——名叫"背景"或"Background"（不管当前是哪种语言，
    /// 两种都认，换了界面语言之后以前存的文件照样能读），可见、不透明度 100%、正常模式，
    /// 盖满整张画布，每个像素都不透明而且是同一种 8 位颜色——就把它从列表里拿掉，返回它的颜色。
    pub fn take_background(layers: &mut Vec<Layer>, canvas: Rect) -> Option<[u8; 3]> {
        let bottom = layers.first()?;
        let p = &bottom.props;
        // 只有最外层、没有蒙版的普通像素图层才可能是代表背景的那一个。
        if !bottom.is_pixels() || bottom.depth != 0 || bottom.mask.is_some() || p.clip {
            return None;
        }
        if !BACKGROUND_NAMES.contains(&p.name.as_str()) || !p.visible || p.opacity < 1.0 || p.blend != BlendMode::Normal {
            return None;
        }
        let first: [u16; 4] = *bottom.tiles.get((0, 0))?.0.first_chunk()?;
        let [r, g, b, a] = to_straight8(first);
        // 16 位的文件里可能有 8 位表示不了的颜色，那样的图层原样留着，不当背景。
        if first[3] as u32 != ONE || to_premultiplied([r, g, b, a].map(|v| v as u16 * 257)) != first {
            return None;
        }
        for (tx, ty) in tiles_in(canvas) {
            let tile = bottom.tiles.get((tx, ty))?;
            let (ox, oy) = (tx * TILE, ty * TILE);
            let clip = Rect { x0: ox, y0: oy, x1: ox + TILE, y1: oy + TILE }.intersect(canvas);
            for y in clip.y0..clip.y1 {
                let at = (((y - oy) * TILE + (clip.x0 - ox)) * 4) as usize;
                let row = &tile.0[at..at + ((clip.x1 - clip.x0) * 4) as usize];
                if row.as_chunks::<4>().0.iter().any(|px| *px != first) {
                    return None;
                }
            }
        }
        layers.remove(0);
        Some([r, g, b])
    }

    // ---- 保存 ----

    /// 先把文件写进 `path` 旁边的临时文件（由 `write` 来写，写完要落盘并关闭文件），成功之后再改名顶替 `path`。
    /// 中途失败不会留下半截文件，原来的文件也原样还在。和 `format` 模块里的做法一样。
    pub fn save_atomically(path: &Path, write: impl FnOnce(File) -> io::Result<()>) -> io::Result<()> {
        let (tmp, file) = create_temp(path)?;
        let result = write(file).and_then(|()| fs::rename(&tmp, path));
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

    // ---- 给用户看的说明 ----
    //
    // 说明都按读文件时的界面语言来写：`kind` 是已经选好语言的那句话，列图层名的格式也跟着语言走。

    /// 读文件时攒下来的说明：哪些内容被忽略了、哪些是近似处理的。
    /// 同一类问题只出一句话，后面列出涉及的图层，免得图层一多就刷出几十条。
    #[derive(Default)]
    pub struct Notes {
        plain: Vec<String>,
        /// 每类问题和涉及的图层名（还没加引号）
        by_kind: Vec<(&'static str, Vec<String>)>,
    }

    impl Notes {
        /// 一句与具体图层无关的说明。重复的只留一句。
        pub fn add(&mut self, text: impl Into<String>) {
            let text = text.into();
            if !self.plain.contains(&text) {
                self.plain.push(text);
            }
        }

        /// 名叫 `name` 的图层有 `kind` 这类问题。
        pub fn layer(&mut self, kind: &'static str, name: &str) {
            let name = format!("{name:.60}");
            match self.by_kind.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, names)) => names.push(name),
                None => self.by_kind.push((kind, vec![name])),
            }
        }

        /// 全部说明，每条一句。列图层名的格式：中文是 `前缀：“甲”、“乙”`，英文是 `Prefix: "a", "b"`。
        pub fn into_vec(self) -> Vec<String> {
            /// 一句话里最多列出的图层个数
            const SHOWN: usize = 6;
            let mut out = self.plain;
            for (kind, names) in self.by_kind {
                let quoted: Vec<String> = names.iter().take(SHOWN).map(|n| trf!("“{n}”", "\"{n}\"")).collect();
                let listed = quoted.join(tr("、", ", "));
                let total = names.len();
                out.push(if total > SHOWN {
                    trf!("{kind}：{listed} 等 {total} 个图层", "{kind}: {listed} and others ({total} layers)")
                } else {
                    trf!("{kind}：{listed}", "{kind}: {listed}")
                });
            }
            out
        }
    }

    // 两种格式共用的几句说明。

    /// 图层伸出画布太远（四周各超过一个画布的宽或高），太远的部分被裁掉了（`Notes::layer` 的 `kind`）。
    pub fn clipped_kind() -> &'static str {
        tr(
            "下列图层伸出画布太远（超过一个画布的宽或高），太远的部分已裁掉",
            "Cropped the parts of these layers that reach more than a canvas width or height beyond the canvas",
        )
    }

    /// 图层的混合模式本程序没有，改成了"正常"（`Notes::layer` 的 `kind`）。
    pub fn unknown_blend_kind() -> &'static str {
        tr(
            "下列图层的混合模式本程序没有，已改为“正常”",
            "Layers whose blend mode this program does not have were set to Normal",
        )
    }

    /// 测试里两个模块都用得上的东西。
    #[cfg(test)]
    pub mod testing {
        use super::*;
        use crate::document::LayerProps;

        /// 测试用的临时文件或目录，离开作用域时删掉（断言失败时也一样）。
        pub struct Temp(pub PathBuf);

        impl Temp {
            pub fn new(tag: &str) -> Self {
                static N: AtomicU32 = AtomicU32::new(0);
                let name = format!("zhibi-interchange-test-{}-{}-{tag}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
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
        pub struct Rng(pub u32);

        impl Rng {
            pub fn next(&mut self) -> u32 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 17;
                self.0 ^= self.0 << 5;
                self.0
            }

            /// 一个合法的预乘像素，偏向容易出错的值：全透明、透明度极低、不透明、颜色等于透明度。
            pub fn pixel(&mut self) -> [u16; 4] {
                let a = match self.next() % 8 {
                    0 => 0,
                    1 => 1 + self.next() % 300,
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

            /// 一个 8 位、非预乘的像素，同样偏向容易出错的值。
            pub fn pixel8(&mut self) -> [u8; 4] {
                let a = match self.next() % 8 {
                    0 => 0,
                    1 => 1,
                    2 => 2,
                    3 | 4 => 255,
                    _ => (self.next() % 256) as u8,
                };
                let v = self.next().to_le_bytes();
                [v[0], v[1], v[2], a]
            }
        }

        pub fn pixel(layer: &Layer, x: i32, y: i32) -> [u16; 4] {
            let (tx, ty) = (x.div_euclid(TILE), y.div_euclid(TILE));
            layer.tiles.get((tx, ty)).map_or([0; 4], |t| {
                let i = ((y - ty * TILE) * TILE + (x - tx * TILE)) as usize * 4;
                [t.0[i], t.0[i + 1], t.0[i + 2], t.0[i + 3]]
            })
        }

        pub fn set_pixel(layer: &mut Layer, x: i32, y: i32, px: [u16; 4]) {
            let (tx, ty) = (x.div_euclid(TILE), y.div_euclid(TILE));
            let i = ((y - ty * TILE) * TILE + (x - tx * TILE)) as usize * 4;
            layer.tiles.get_mut((tx, ty)).0[i..i + 4].copy_from_slice(&px);
        }

        /// 图层在整张画布上的像素，写成 8 位、非预乘的 RGBA（也就是存进交换格式之后该有的样子）。
        pub fn straight8(layer: &Layer, canvas: Rect) -> Vec<u8> {
            let w = (canvas.x1 - canvas.x0) as usize;
            let mut out = vec![0u8; w * (canvas.y1 - canvas.y0) as usize * 4];
            for (y, row) in (canvas.y0..canvas.y1).zip(out.chunks_exact_mut(w * 4)) {
                read_row(&layer.tiles, y, canvas.x0, canvas.x1, row);
            }
            out
        }

        /// 尺寸不是 64 的倍数的文档，八种混合模式各有一个图层，各种麻烦的情况都放了一点。
        pub fn sample_document() -> Document {
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
            set_pixel(&mut paint, 0, 0, [200, 0, 200, 200]);
            set_pixel(&mut paint, w - 1, 0, [ONE as u16; 4]);
            set_pixel(&mut paint, 0, h - 1, [300, 200, 100, 500]);
            set_pixel(&mut paint, w - 1, h - 1, [0, 0, 0, 100]);
            // 右边和下边的分块伸出画布，外面那部分也有内容：伸出画布的内容照样存（实现方案 §3.15）。
            for y in 0..192 {
                for x in 0..256 {
                    if x >= w || y >= h {
                        set_pixel(&mut paint, x, y, [20000, 12345, 1, 30000]);
                    }
                }
            }
            // 还有整个在画布外的分块。
            // 还有整个在画布外的分块：左边那块在图层能放到的范围里，右下那块离画布太远。
            paint.tiles.get_mut((7, 7)).0.fill(4000);
            paint.tiles.get_mut((-1, 0)).0.fill(8000);

            // 空图层。有一个全透明的分块、一个画布外的分块、一个透明度低到 8 位下看不见的像素，仍然算空。
            let mut empty = Layer::new("空图层");
            empty.props = LayerProps { opacity: 0.37, visible: false, blend: BlendMode::Multiply, ..empty.props };
            empty.tiles.get_mut((1, 1));
            empty.tiles.get_mut((9, 0)).0.fill(ONE as u16);
            set_pixel(&mut empty, 5, 5, [10, 20, 30, 64]);

            // 离原点有一段距离的一小块，跨了好几个分块；名字里有 BMP 之外的字符和 XML 的特殊字符。
            let mut lines = Layer::new("线稿 ✏️ <a&b> \"引号\" 𝄞");
            lines.props = LayerProps { opacity: 0.8125, blend: BlendMode::Overlay, ..lines.props };
            for y in 40..101 {
                for x in 70..91 {
                    let a = (65 + (x * 331 + y * 577) % (ONE as i32 - 64)) as u16;
                    set_pixel(&mut lines, x, y, [a / 2, a, 0, a]);
                }
            }

            let mut layers = vec![paint, empty, lines];
            // 其余几种混合模式各来一个小图层，位置各不相同，有的贴着画布的边。
            let rest = [BlendMode::Screen, BlendMode::Darken, BlendMode::Lighten, BlendMode::Add, BlendMode::Difference];
            for (k, mode) in rest.into_iter().enumerate() {
                let mut layer = Layer::new(&format!("{} {k}", mode.label()));
                layer.props = LayerProps { opacity: 1.0 - 0.15 * k as f32, visible: k != 3, blend: mode, ..layer.props };
                let (x0, y0) = (w - 30 - 35 * k as i32, 15 * k as i32);
                for y in y0..y0 + 70 {
                    for x in x0..x0 + 30 {
                        set_pixel(&mut layer, x, y, rng.pixel());
                    }
                }
                set_pixel(&mut layer, x0, y0, [ONE as u16, 0, 0, ONE as u16]);
                set_pixel(&mut layer, x0 + 29, y0 + 69, [0, 0, ONE as u16, ONE as u16]);
                layers.push(layer);
            }

            let mut doc = Document::from_layers(w, h, [12, 200, 77], layers);
            doc.dpi = 144;
            doc
        }

        /// 存了再读回来之后该有的样子：图层个数、属性、内容的位置、背景色、分辨率都不变，
        /// 像素按 8 位算一模一样。`opacity_step` 是这种格式存不透明度的精度。
        pub fn assert_same_after_round_trip(doc: &Document, back: &Document, opacity_step: f32) {
            assert_eq!((back.width, back.height, back.dpi, back.background), (doc.width, doc.height, doc.dpi, doc.background));
            assert_eq!(back.layers().len(), doc.layers().len());
            assert!(!back.is_modified() && !back.can_undo() && !back.can_redo());
            let canvas = doc.bounds();
            for (a, b) in doc.layers().iter().zip(back.layers()) {
                let name = &a.props.name;
                assert_eq!((&b.props.name, b.props.visible, b.props.blend), (name, a.props.visible, a.props.blend));
                assert!((a.props.opacity - b.props.opacity).abs() <= opacity_step, "图层“{name}”的不透明度");
                assert_eq!(content_bounds(&b.tiles, canvas), content_bounds(&a.tiles, canvas), "图层“{name}”的位置");
                assert!(straight8(a, canvas) == straight8(b, canvas), "图层“{name}”的像素");
            }
        }

        /// 在英文界面下执行 `f`，完了把语言换回中文（`f` 里断言失败也一样）。
        pub fn in_english<T>(f: impl FnOnce() -> T) -> T {
            struct BackToChinese;
            impl Drop for BackToChinese {
                fn drop(&mut self) {
                    crate::lang::set_english(false);
                }
            }
            let _back = BackToChinese;
            crate::lang::set_english(true);
            f()
        }

        // ---- 和别的程序互相读写的核对 ----
        //
        // 这几个函数给两个模块里标了 `#[ignore]` 的 `interop_*` 测试用，平时不跑。
        // 核对的办法：把文档的内容（图层属性，加上每个图层整张画布的 8 位像素）存成文件，
        // 让别的程序（GIMP）也把它读到的东西按同样的格式存一份，再逐项、逐像素地比。
        // GIMP 那一侧是在它的批处理模式（`gimp-console -i --batch-interpreter=python-fu-eval -b …`）里跑的
        // Python 脚本，和对比用的脚本一起放在 `runs/interop/scripts/`；那个目录不入库。

        /// 两个模块的 `load` 返回的东西：文档和说明。
        pub type Loaded = io::Result<(Document, Vec<String>)>;

        /// 核对用的文件放在哪个目录：环境变量 `ZHIBI_INTEROP_DIR`。没设就什么都不做。
        pub fn interop_dir() -> Option<PathBuf> {
            std::env::var_os("ZHIBI_INTEROP_DIR").map(PathBuf::from)
        }

        /// 把读到的文档（或者读的时候报的错）存成 `<prefix>.json`，各图层的像素存成 `<prefix>.layerNN.rgba`
        /// （整张画布，8 位、非预乘的 RGBA）。图层自下而上排列。
        pub fn dump_for_interop(result: &Loaded, prefix: &Path) {
            let with_suffix = |suffix: &str| {
                let mut name = prefix.as_os_str().to_owned();
                name.push(suffix);
                PathBuf::from(name)
            };
            let info = match result {
                Err(e) => serde_json::json!({ "error": e.to_string() }),
                Ok((doc, notes)) => {
                    let canvas = doc.bounds();
                    let mut layers = Vec::new();
                    for (i, layer) in doc.layers().iter().enumerate() {
                        let file = with_suffix(&format!(".layer{i:02}.rgba"));
                        fs::write(&file, straight8(layer, canvas)).unwrap();
                        let r = content_bounds(&layer.tiles, canvas);
                        layers.push(serde_json::json!({
                            "name": layer.props.name,
                            "opacity": layer.props.opacity,
                            "visible": layer.props.visible,
                            "mode": layer.props.blend.id(),
                            "x": r.x0,
                            "y": r.y0,
                            "w": r.x1 - r.x0,
                            "h": r.y1 - r.y0,
                            "file": file.file_name().unwrap().to_string_lossy(),
                        }));
                    }
                    serde_json::json!({
                        "width": doc.width,
                        "height": doc.height,
                        "dpi": doc.dpi,
                        "background": doc.background,
                        "notes": notes,
                        "layers": layers,
                    })
                }
            };
            fs::write(with_suffix(".json"), serde_json::to_string_pretty(&info).unwrap()).unwrap();
        }

        /// 用 `load` 读目录里所有扩展名是 `extension` 的文件（本程序自己写的 `ours.*` 除外），
        /// 把读到的内容存成 `<文件名>.loaded.*`。
        pub fn interop_load_all(extension: &str, load: fn(&Path) -> Loaded) {
            let Some(dir) = interop_dir() else { return };
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.extension().is_some_and(|e| e == extension) && !name.starts_with("ours.") {
                    let result = load(&path);
                    match &result {
                        Ok((doc, notes)) => println!("{name}: {} 个图层，说明 {notes:#?}", doc.layers().len()),
                        Err(e) => println!("{name}: 读不了：{e}"),
                    }
                    dump_for_interop(&result, &dir.join(format!("{name}.loaded")));
                    // 合成之后的样子，和别的程序自己合成的对照。
                    if let Ok((doc, _)) = &result {
                        doc.export_png(&dir.join(format!("{name}.loaded.png"))).unwrap();
                        let tree: Vec<String> = doc
                            .layers()
                            .iter()
                            .rev()
                            .map(|l| format!("{}{} {:?} {:.2} {:?}{}{}", "  ".repeat(l.depth as usize), l.props.name, l.props.blend, l.props.opacity, l.kind, if l.props.pass_through { " 穿透" } else { "" }, if l.mask.is_some() { " 蒙版" } else { "" }))
                            .collect();
                        println!("{}", tree.join("\n"));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shared::testing::*;
    use super::shared::{to_premultiplied, to_straight8};
    use super::*;
    use crate::document::Mask;
    use crate::tile::{GrayMap, TILE};
    use crate::ONE;

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

    /// 8 位 RGBA 的 PNG，所有像素都是 `px`（非预乘）。
    fn png8(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        encode_png(w, h, png::ColorType::Rgba, png::BitDepth::Eight, &px.repeat((w * h) as usize))
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

    /// 把 `stack.xml` 的文本和几张图层图像打成压缩包再读。
    fn load_parts(xml: &str, images: &[(&str, &[u8])]) -> io::Result<(Document, Vec<String>)> {
        let tmp = Temp::new("parts.ora");
        let mut entries: Vec<(&str, &[u8])> = vec![(MIMETYPE, MIMETYPE_CONTENT.as_bytes()), (STACK_XML, xml.as_bytes())];
        entries.extend_from_slice(images);
        write_zip(&tmp.0, &entries);
        load(&tmp.0)
    }

    /// 必须是 `InvalidData`，而且错误信息说的是预期的那件事。
    fn expect_invalid(result: io::Result<(Document, Vec<String>)>, keyword: &str) {
        let e = result.err().expect("应当报错");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        assert!(e.to_string().contains(keyword), "错误信息里应当有“{keyword}”：{e}");
    }

    fn expect_note(notes: &[String], keyword: &str) {
        assert!(notes.iter().any(|n| n.contains(keyword)), "说明里应当有“{keyword}”：{notes:?}");
    }

    #[test]
    fn round_trip_keeps_everything_up_to_eight_bits() {
        let doc = sample_document();
        let tmp = Temp::new("roundtrip.ora");
        save(&doc, &tmp.0).unwrap();
        let (back, notes) = load(&tmp.0).unwrap();
        assert_eq!(notes, Vec::<String>::new());
        // 不透明度是按十进制小数原样写的，一点不差。
        assert_same_after_round_trip(&doc, &back, 0.0);
        assert_eq!(back.active(), doc.layers().len() - 1);
        // 伸出画布的内容跟过来了（只差 8 位量化），离画布太远的没有；空图层读回来一个分块也没有。
        assert!(back.layers()[0].tiles.get((7, 7)).is_none());
        for (x, y) in [(-64, 0), (-1, 63), (255, 191)] {
            let (a, b) = (pixel(&doc.layers()[0], x, y), pixel(&back.layers()[0], x, y));
            assert!(a.iter().zip(b).all(|(p, q)| p.abs_diff(q) <= 130), "({x}, {y})：{a:?} 对 {b:?}");
        }
        assert!(back.layers()[1].tiles.is_empty());
        // 合成的结果只差在 8 位量化上。
        for (a, b) in doc.flatten_rgb().iter().zip(back.flatten_rgb()) {
            assert!(a.abs_diff(b) <= 2, "{a} vs {b}");
        }

        // 再存一次、再读一次，文档一点不变：第一次存盘已经把像素量化成 8 位了。
        let again = Temp::new("again.ora");
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
    }

    /// 写出来的文件确实是规范要求的样子。
    #[test]
    fn written_file_follows_the_specification() {
        let doc = sample_document();
        let tmp = Temp::new("layout.ora");
        save(&doc, &tmp.0).unwrap();
        let bytes = std::fs::read(&tmp.0).unwrap();

        // mimetype 是第一个条目，不压缩，没有额外字段：文件名从第 30 个字节开始，内容紧跟其后。
        assert_eq!(&bytes[..4], b"PK\x03\x04");
        assert_eq!(&bytes[8..10], [0, 0], "压缩方式应为 0（原样存放）");
        assert_eq!(&bytes[26..30], [8, 0, 0, 0], "文件名 8 个字节，没有额外字段");
        assert_eq!(&bytes[30..54], b"mimetypeimage/openraster");

        let mut zip = ZipArchive::new(File::open(&tmp.0).unwrap()).unwrap();
        let names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_string()).collect();
        let mut expect = vec!["mimetype".to_string(), "stack.xml".to_string()];
        expect.extend((0..8).map(|i| format!("data/layer{i:03}.png")));
        expect.extend(["data/background.png", "mergedimage.png", "Thumbnails/thumbnail.png"].map(String::from));
        assert_eq!(names, expect);

        // stack.xml：能被解析，根元素和图层的属性都对，最上面的图层在最前。
        let xml = String::from_utf8(read_entry(&tmp.0, STACK_XML)).unwrap();
        assert!(xml.starts_with("<?xml version='1.0' encoding='UTF-8'?>\n<image "), "{xml}");
        let root = parse_xml(&xml).unwrap();
        let attrs =
            |el: &Element, keys: &[&str]| -> Vec<String> { keys.iter().map(|k| el.attr(k).unwrap().to_string()).collect() };
        assert_eq!(attrs(&root, &["version", "w", "h", "xres", "yres"]), ["0.0.3", "200", "130", "144", "144"]);
        let stack = &root.children[0];
        assert_eq!((root.children.len(), stack.name.as_str(), stack.children.len()), (1, "stack", 9));
        let keys = ["name", "src", "x", "y", "opacity", "visibility", "composite-op"];
        assert_eq!(
            attrs(&stack.children[5], &keys),
            ["线稿 ✏️ <a&b> \"引号\" 𝄞", "data/layer002.png", "70", "40", "0.8125", "visible", "svg:overlay"]
        );
        assert_eq!(attrs(&stack.children[6], &keys), ["空图层", "data/layer001.png", "0", "0", "0.37", "hidden", "svg:multiply"]);
        assert_eq!(
            attrs(&stack.children[7], &keys),
            ["底色 paint", "data/layer000.png", "-64", "0", "1", "visible", "svg:src-over"]
        );
        assert_eq!(attrs(&stack.children[8], &keys), ["背景", "data/background.png", "0", "0", "1", "visible", "svg:src-over"]);
        assert_eq!(attrs(&stack.children[0], &["name", "composite-op"]), ["差值 4", "svg:difference"]);
        assert_eq!(attrs(&stack.children[1], &["visibility", "composite-op"]), ["hidden", "svg:plus"]);
        assert!(xml.contains("&lt;a&amp;b&gt; &quot;引号&quot;"), "{xml}");

        // 图层图像：8 位 RGBA、非预乘，只有内容所在的那块；空图层是一张 1×1 的全透明图。
        let rgba8 = (png::ColorType::Rgba, png::BitDepth::Eight);
        let (info, data) = decode_png(&read_entry(&tmp.0, "data/layer002.png"));
        assert_eq!((info.width, info.height, (info.color_type, info.bit_depth)), (21, 61, rgba8));
        let lines = &doc.layers()[2];
        assert_eq!(data[..4], to_straight8(pixel(lines, 70, 40)));
        assert_eq!(data[data.len() - 4..], to_straight8(pixel(lines, 90, 100)));
        let (info, data) = decode_png(&read_entry(&tmp.0, "data/layer000.png"));
        // 伸出画布的部分也写了：左边那一块（−64..0），右边和下边伸出去的（到 256、192）。
        assert_eq!((info.width, info.height, (info.color_type, info.bit_depth)), (320, 192, rgba8));
        assert!(data == straight8(&doc.layers()[0], Rect { x0: -64, y0: 0, x1: 256, y1: 192 }));
        let (info, data) = decode_png(&read_entry(&tmp.0, "data/layer001.png"));
        assert_eq!((info.width, info.height, data), (1, 1, vec![0; 4]));
        let (info, data) = decode_png(&read_entry(&tmp.0, BACKGROUND_PNG));
        assert_eq!((info.width, info.height, (info.color_type, info.bit_depth)), (200, 130, rgba8));
        assert!(data.as_chunks::<4>().0.iter().all(|px| *px == [12, 200, 77, 255]));

        // 合成图和缩略图。
        let rgb8 = (png::ColorType::Rgb, png::BitDepth::Eight);
        let (info, merged) = decode_png(&read_entry(&tmp.0, MERGED_PNG));
        assert_eq!((info.width, info.height, (info.color_type, info.bit_depth)), (200, 130, rgb8));
        assert_eq!(merged, doc.flatten_rgb());
        let (info, thumb) = decode_png(&read_entry(&tmp.0, THUMBNAIL_PNG));
        assert_eq!((info.width, info.height, (info.color_type, info.bit_depth)), (200, 130, rgb8));
        assert_eq!(thumb, merged);
    }

    #[test]
    fn thumbnail_is_at_most_256_pixels() {
        // 左半边是不透明的红色，右半边露出背景。
        let mut left = Layer::new("左半");
        for y in 0..300 {
            for x in 0..300 {
                set_pixel(&mut left, x, y, [ONE as u16, 0, 0, ONE as u16]);
            }
        }
        let doc = Document::from_layers(600, 300, [10, 20, 30], vec![left]);
        let tmp = Temp::new("thumb.ora");
        save(&doc, &tmp.0).unwrap();
        let (info, thumb) = decode_png(&read_entry(&tmp.0, THUMBNAIL_PNG));
        assert_eq!(
            (info.width, info.height, info.color_type, info.bit_depth),
            (256, 128, png::ColorType::Rgb, png::BitDepth::Eight)
        );
        let at = |x: usize, y: usize| &thumb[(y * 256 + x) * 3..][..3];
        assert_eq!(
            (at(0, 0), at(127, 64), at(128, 64), at(255, 127)),
            (&[255, 0, 0][..], &[255, 0, 0][..], &[10, 20, 30][..], &[10, 20, 30][..])
        );
        assert_eq!(thumbnail(&vec![7; 16384 * 3], 16384, 1), (vec![7; 256 * 3], 256, 1));
        assert_eq!(thumbnail(&[9; 100 * 50 * 3], 100, 50), (vec![9; 100 * 50 * 3], 100, 50));
    }

    /// 8 位 → 分块 → 8 位：透明度不小于 2/255 的像素一位不差。
    #[test]
    fn eight_bit_pixels_survive_the_trip_through_tiles() {
        let import = |px: [u8; 4]| to_premultiplied(px.map(|v| v as u16 * 257));
        for a in 0..=255u8 {
            for c in 0..=255u8 {
                let px = [c, 255 - c, c / 2, a];
                let tile = import(px);
                let back = to_straight8(tile);
                match a {
                    0 => assert_eq!((tile, back), ([0; 4], [0; 4])),
                    // 透明度 1/255：颜色最多差 1，而且写出去之后就不再变了。
                    1 => {
                        assert!(back[3] == 1 && (0..3).all(|i| back[i].abs_diff(px[i]) <= 1), "{px:?} -> {back:?}");
                        assert_eq!((import(back), to_straight8(import(back))), (tile, back));
                    }
                    _ => assert_eq!(back, px, "{px:?} -> {tile:?}"),
                }
            }
        }
        // 不透明的像素显示出来就是文件里的颜色。
        assert_eq!(import([255, 128, 1, 255]), [ONE as u16, 16448, 129, ONE as u16]);
        // 16 位的文件精度更高，直接换算成 15 位，不先砍成 8 位。
        assert_eq!(to_premultiplied([65535, 32768, 2, 65535]), [ONE as u16, 16384, 1, ONE as u16]);
        assert_eq!(to_premultiplied([65535, 0, 0, 32768]), [16384, 0, 0, 16384]);
    }

    /// 文件夹存成嵌套的 <stack>，读回来还是同样的文件夹；蒙版乘进像素、填充图层画成像素，合成的样子不变。
    #[test]
    fn folders_survive_and_masks_and_fills_are_baked() {
        let doc = crate::psd::tests::tree_document();
        let mut layers = doc.layers().to_vec();
        // 外层文件夹再加一个左边白、右边黑的蒙版：非黑即白的蒙版乘进每个图层和先合成再乘是一样的。
        let mut m = Mask { tiles: GrayMap::default(), default: 0, disabled: false };
        for y in 0..doc.height {
            for x in 0..70 {
                m.tiles.get_mut((x / TILE, y / TILE), 0)[((y % TILE) * TILE + x % TILE) as usize] = ONE as u16;
            }
        }
        layers[3].mask = Some(m);
        let doc = Document::from_layers(doc.width, doc.height, doc.background, layers);
        let tmp = Temp::new("tree.ora");
        save(&doc, &tmp.0).unwrap();
        let xml = String::from_utf8(read_entry(&tmp.0, "stack.xml")).unwrap();
        assert!(xml.contains(r#"<stack name="外" opacity="0.8" visibility="visible" composite-op="svg:multiply" isolation="isolate">"#), "{xml}");
        assert!(xml.contains(r#"<stack name="里" opacity="1" visibility="hidden" composite-op="svg:src-over" isolation="auto">"#), "{xml}");

        let (back, notes) = load(&tmp.0).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(back.layers().len(), doc.layers().len());
        for (x, y) in doc.layers().iter().zip(back.layers()) {
            let name = &x.props.name;
            assert_eq!((&y.props.name, y.depth, y.is_folder()), (name, x.depth, x.is_folder()), "{name}");
            assert!(y.mask.is_none() && (y.is_folder() || y.is_pixels()), "{name}");
            assert_eq!((y.props.visible, y.props.blend, y.props.pass_through), (x.props.visible, x.props.blend, x.props.pass_through), "{name}");
            assert_eq!(y.props.opacity, x.props.opacity, "{name}");
        }
        // 合成的样子只差 8 位的舍入。
        let (a, b) = (doc.flatten_rgb(), back.flatten_rgb());
        let worst = a.iter().zip(&b).map(|(p, q)| p.abs_diff(*q)).max().unwrap();
        assert!(worst <= 1, "最多差 {worst}");
        // 读回来的文档再存一遍，文件里的东西一样。
        let again = Temp::new("tree-again.ora");
        save(&back, &again.0).unwrap();
        assert_eq!(read_entry(&again.0, "stack.xml"), read_entry(&tmp.0, "stack.xml"));
    }

    /// 分块 → 8 位 → 分块 → 8 位：任何像素写出去之后，读回来再写结果都一样。
    #[test]
    fn export_reaches_a_fixed_point_after_one_save() {
        let check = |px: [u16; 4]| {
            let first = to_straight8(px);
            let again = to_straight8(to_premultiplied(first.map(|v| v as u16 * 257)));
            assert_eq!(again, first, "{px:?}");
        };
        for a in 0..=ONE as u16 {
            // 透明度很低的地方颜色取遍所有值，其余的抽样。
            if a <= 700 {
                for c in 0..=a {
                    check([c, a - c, c / 2, a]);
                }
            } else {
                for c in [0, 1, 2, a / 3, a / 2 - 1, a / 2, a / 2 + 1, a - 2, a - 1, a] {
                    check([c, a - c, c / 2, a]);
                }
                for c in (0..=a).step_by(251) {
                    check([c, a - c, a, a]);
                }
            }
        }
        assert_eq!(to_straight8([ONE as u16; 4]), [255; 4]);
        assert_eq!(to_straight8([8192, 0, 16384, 16384]), [128, 0, 255, 128]);
        // 透明度低到 8 位下是 0 的像素整个变成透明的。
        assert_eq!(to_straight8([64, 0, 64, 64]), [0; 4]);
        assert_eq!(to_straight8([65, 0, 65, 65]), [255, 0, 255, 1]);
        // 不合法的预乘值（颜色大于透明度、透明度大于 1）夹到范围内，不溢出。
        assert_eq!(to_straight8([40000, 5, 70, 300]), [255, 4, 60, 2]);
        assert_eq!(to_straight8([65535; 4]), [255; 4]);
    }

    /// 8 位的图像读进来、写出去、再读进来，两次读到的东西完全一样；写出去的文件里像素也没变。
    #[test]
    fn an_eight_bit_image_is_stable_through_import_and_export() {
        let (w, h) = (150u32, 70u32);
        let mut rng = Rng(0x1234_5678);
        let mut source = Vec::new();
        for _ in 0..w * h {
            source.extend(rng.pixel8());
        }
        let png = encode_png(w, h, png::ColorType::Rgba, png::BitDepth::Eight, &source);
        let xml = format!(r#"<image w="{w}" h="{h}"><stack><layer name="导入" src="a.png"/></stack></image>"#);
        let (first, notes) = load_parts(&xml, &[("a.png", &png)]).unwrap();
        assert_eq!(notes, Vec::<String>::new());

        let tmp = Temp::new("stable.ora");
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
        for (src, out) in source.as_chunks::<4>().0.iter().zip(exported.as_chunks::<4>().0) {
            match src[3] {
                0 => assert_eq!(*out, [0; 4]),
                1 => assert!(out[3] == 1 && (0..3).all(|i| out[i].abs_diff(src[i]) <= 1), "{src:?} -> {out:?}"),
                _ => assert_eq!(out, src),
            }
        }
    }

    /// 一个别的程序可能写出来的文件：有图层组、偏移量、各种 PNG 类型、不认识的属性和元素。
    #[test]
    fn loads_a_file_written_by_another_tool() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<!-- 别的程序写的 -->
<image version="0.0.6" w="100" h="80" xres="150" yres="150" made-by='某个 "程序"'>
  <stack opacity="1" name="root">
    <layer name="顶层 &amp; 16 位" src="data/top.png" x="-3" y="70" opacity="0.5" composite-op="svg:multiply" mypaint:extra="1"/>
    <stack name="组" opacity="0.5" visibility="hidden" composite-op="svg:screen">
      <layer name="组里的灰度" src="data/grey.png" x="10" y="10" opacity="0.5"></layer>
      <stack name="内层组" isolation="auto" composite-op="svg:difference">
        <layer src="data/palette.png" x="98" y="0" visibility="visible" composite-op="svg:color-dodge"/>
      </stack>
      <text x="1" y="2">这种元素不认识</text>
    </stack>
    <layer name="RGB" src="data/rgb.png" visibility="hidden" composite-op="svg:plus" x="0" y="0" selected="true"/>
    <layer name="矢量" src="data/drawing.svg"/>
  </stack>
</image>
"#;
        // 16 位 RGBA，半透明的红；放在 (-3, 70)，10×20，左边和下边都伸出画布。
        let top = [0xFFu8, 0xFF, 0, 0, 0, 0, 0x80, 0x00].repeat(10 * 20);
        let top = encode_png(10, 20, png::ColorType::Rgba, png::BitDepth::Sixteen, &top);
        // 8 位灰度加透明度。
        let grey = encode_png(
            4,
            2,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            &[100, 255, 100, 255, 0, 0, 200, 128].repeat(2),
        );
        // 调色板图像，带 tRNS：0 号颜色全透明，1 号是不透明的蓝。4×1，放在 x=98，右边两个像素出界。
        let palette = {
            let mut out = Vec::new();
            let mut enc = png::Encoder::new(&mut out, 4, 1);
            enc.set_color(png::ColorType::Indexed);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_palette(vec![9, 9, 9, 0, 0, 255]);
            enc.set_trns(vec![0]);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&[1, 0, 0, 1]).unwrap();
            writer.finish().unwrap();
            out
        };
        // 8 位 RGB，没有透明度：盖满画布的橙色。
        let rgb = encode_png(100, 80, png::ColorType::Rgb, png::BitDepth::Eight, &[255, 128, 0].repeat(100 * 80));
        let images: [(&str, &[u8]); 5] = [
            ("data/top.png", &top),
            ("data/grey.png", &grey),
            ("data/palette.png", &palette),
            ("data/rgb.png", &rgb),
            ("data/drawing.svg", b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
        ];
        let (doc, notes) = load_parts(xml, &images).unwrap();

        assert_eq!((doc.width, doc.height, doc.dpi, doc.background), (100, 80, 150, [255; 3]));
        let props: Vec<_> = doc
            .layers()
            .iter()
            .map(|l| (l.props.name.as_str(), l.depth, l.is_folder(), l.props.pass_through, l.props.opacity, l.props.visible, l.props.blend))
            .collect();
        assert_eq!(
            props,
            [
                ("RGB", 0, false, false, 1.0, false, BlendMode::Add),
                // 没有名字的图层按它在图层里自下而上的位置起名（跳过的那个矢量图层也占一个位置，文件夹不占）。
                ("图层 3", 2, false, false, 1.0, true, BlendMode::Normal),
                // isolation="auto" 的组是穿透的，规范说这时组的混合模式不起作用；
                // 没写 isolation 的组按规范是普通文件夹（isolate），带着自己的混合模式。
                ("内层组", 1, true, true, 1.0, true, BlendMode::Normal),
                ("组里的灰度", 1, false, false, 0.5, true, BlendMode::Normal),
                ("组", 0, true, false, 0.5, false, BlendMode::Screen),
                ("顶层 & 16 位", 0, false, false, 0.5, true, BlendMode::Multiply),
            ]
        );
        let layer = |i: usize| &doc.layers()[[0, 1, 3, 5][i]];
        let orange = to_premultiplied([255u16 * 257, 128 * 257, 0, 65535]);
        assert_eq!((pixel(layer(0), 0, 0), pixel(layer(0), 99, 79)), (orange, orange));
        // 调色板图层：只有 x=98 那个蓝色像素留了下来（x=99 是透明的，再往右出界了）。
        assert_eq!(pixel(layer(1), 98, 0), [0, 0, ONE as u16, ONE as u16]);
        assert_eq!(content_bounds(&layer(1).tiles, doc.bounds()), Rect { x0: 98, y0: 0, x1: 99, y1: 1 });
        // 灰度图层：(10,10) 起 4×2。
        assert_eq!(pixel(layer(2), 10, 10), to_premultiplied([100 * 257, 100 * 257, 100 * 257, 65535]));
        assert_eq!(pixel(layer(2), 12, 11), [0; 4]);
        assert_eq!(pixel(layer(2), 13, 11), to_premultiplied([200 * 257, 200 * 257, 200 * 257, 128 * 257]));
        // 16 位图层：裁到画布之内的是 x 0..7、y 70..80。
        assert_eq!(content_bounds(&layer(3).tiles, doc.bounds()), Rect { x0: 0, y0: 70, x1: 7, y1: 80 });
        assert_eq!(pixel(layer(3), 6, 79), [16384, 0, 0, 16384]);

        assert_eq!(notes.len(), 3, "{notes:?}");
        expect_note(&notes, "不认识的内容：<text>");
        expect_note(&notes, "不是 PNG，已跳过：“矢量”");
        expect_note(&notes, "混合模式本程序没有，已改为“正常”：“图层 3（svg:color-dodge）”");
        // 伸出画布的部分留着（实现方案 §3.15）：调色板图层右边出界的那个蓝像素，16 位图层左边和下边出界的部分。
        assert_eq!(pixel(layer(1), 101, 0), [0, 0, ONE as u16, ONE as u16]);
        assert_eq!(pixel(layer(3), -3, 89), [16384, 0, 0, 16384]);
    }

    #[test]
    fn background_layer_becomes_the_background_colour_only_when_it_really_is_one() {
        let xml = |name: &str, extra: &str| {
            format!(
                r#"<image w="70" h="66"><stack><layer name="画" src="a.png" x="1" y="1"/><layer name="{name}" src="bg.png" {extra}/></stack></image>"#
            )
        };
        let a = png8(2, 2, [255, 0, 0, 255]);
        let load_with =
            |name: &str, extra: &str, bg: &[u8]| load_parts(&xml(name, extra), &[("a.png", &a), ("bg.png", bg)]).unwrap().0;
        let solid = png8(70, 66, [30, 60, 90, 255]);

        let doc = load_with("背景", "", &solid);
        assert_eq!((doc.background, doc.layers().len(), doc.layers()[0].props.name.as_str()), ([30, 60, 90], 1, "画"));
        // 图像比画布大、带偏移量，只要盖满画布就行。
        let doc = load_with("背景", r#"x="-5" y="-5""#, &png8(80, 80, [1, 2, 3, 255]));
        assert_eq!((doc.background, doc.layers().len()), ([1, 2, 3], 1));
        // 16 位的图像，颜色正好是 8 位能表示的。
        let deep =
            encode_png(70, 66, png::ColorType::Rgb, png::BitDepth::Sixteen, &[0x80, 0x80, 0, 0, 0xFF, 0xFF].repeat(70 * 66));
        assert_eq!(load_with("背景", "", &deep).background, [128, 0, 255]);
        // 英文界面下存的文件里，这个图层叫 "Background"，不管现在是哪种语言都认。
        assert_eq!(load_with("Background", "", &solid).background, [30, 60, 90]);
        assert_eq!(in_english(|| load_with("背景", "", &solid)).background, [30, 60, 90]);

        // 下面这些都不算：照旧是一个普通图层，背景色是白的。
        let kept = |doc: Document| (doc.background, doc.layers().len()) == ([255; 3], 2);
        assert!(kept(load_with("background", "", &solid)), "名字不对");
        assert!(kept(load_with("背景 1", "", &solid)), "名字不对");
        assert!(kept(load_with("背景", r#"visibility="hidden""#, &solid)), "隐藏的");
        assert!(kept(load_with("背景", r#"opacity="0.99""#, &solid)), "半透明的");
        assert!(kept(load_with("背景", r#"composite-op="svg:multiply""#, &solid)), "不是正常模式");
        assert!(kept(load_with("背景", r#"x="1""#, &solid)), "没盖满画布");
        assert!(kept(load_with("背景", "", &png8(70, 65, [30, 60, 90, 255]))), "没盖满画布");
        assert!(kept(load_with("背景", "", &png8(70, 66, [30, 60, 90, 254]))), "不是完全不透明");
        let mut two = [30u8, 60, 90, 255].repeat(70 * 66);
        two[(65 * 70 + 69) * 4] = 31;
        assert!(
            kept(load_with("背景", "", &encode_png(70, 66, png::ColorType::Rgba, png::BitDepth::Eight, &two))),
            "不止一种颜色"
        );
        let odd =
            encode_png(70, 66, png::ColorType::Rgb, png::BitDepth::Sixteen, &[0x80, 0x00, 0, 0, 0xFF, 0xFF].repeat(70 * 66));
        assert!(kept(load_with("背景", "", &odd)), "8 位表示不了的颜色");

        // 只有一个背景图层的文件：背景色还原了，文档里补一个空图层。
        let only = r#"<image w="70" h="66"><stack><layer name="背景" src="bg.png"/></stack></image>"#;
        let (doc, _) = load_parts(only, &[("bg.png", &solid)]).unwrap();
        assert_eq!((doc.background, doc.layers().len()), ([30, 60, 90], 1));
        assert!(doc.layers()[0].tiles.is_empty());
        // 一个图层都没有的文件也能读。
        let (doc, _) = load_parts(r#"<image w="70" h="66"><stack/></image>"#, &[]).unwrap();
        assert_eq!((doc.width, doc.height, doc.dpi, doc.layers().len()), (70, 66, 72, 1));
    }

    /// 背景图层的名字跟着界面语言走；换了语言之后，以前存的文件照样还原出背景色。
    #[test]
    fn background_colour_survives_a_change_of_language() {
        let doc = sample_document();
        let background_layer = |path: &Path| {
            let root = parse_xml(&String::from_utf8(read_entry(path, STACK_XML)).unwrap()).unwrap();
            root.children[0].children.last().unwrap().attr("name").unwrap().to_string()
        };

        // 英文界面下存，英文、中文界面下读。
        let english = Temp::new("english.ora");
        let (back, notes) = in_english(|| {
            save(&doc, &english.0).unwrap();
            load(&english.0).unwrap()
        });
        assert_eq!(background_layer(&english.0), "Background");
        assert_eq!(notes, Vec::<String>::new());
        assert_same_after_round_trip(&doc, &back, 0.0);
        assert_same_after_round_trip(&doc, &load(&english.0).unwrap().0, 0.0);

        // 中文界面下存，英文界面下读。
        let chinese = Temp::new("chinese.ora");
        save(&doc, &chinese.0).unwrap();
        assert_eq!(background_layer(&chinese.0), "背景");
        let (back, notes) = in_english(|| load(&chinese.0)).unwrap();
        assert_eq!(notes, Vec::<String>::new());
        assert_same_after_round_trip(&doc, &back, 0.0);
    }

    /// 说明里列图层名的格式跟着语言走，超过六个的只列前六个。
    #[test]
    fn notes_list_layer_names_in_the_current_language() {
        let collect = || {
            let mut notes = Notes::default();
            notes.add("一句话");
            notes.add("一句话");
            for name in ["a", "b", "a"] {
                notes.layer("甲", name);
            }
            for i in 1..=8 {
                notes.layer("乙", &format!("L{i}"));
            }
            notes.layer("丙", &"长".repeat(100));
            notes.into_vec()
        };
        let long = "长".repeat(60);
        assert_eq!(
            collect(),
            [
                "一句话".to_string(),
                "甲：“a”、“b”、“a”".to_string(),
                "乙：“L1”、“L2”、“L3”、“L4”、“L5”、“L6” 等 8 个图层".to_string(),
                format!("丙：“{long}”"),
            ]
        );
        assert_eq!(
            in_english(collect),
            [
                "一句话".to_string(),
                r#"甲: "a", "b", "a""#.to_string(),
                r#"乙: "L1", "L2", "L3", "L4", "L5", "L6" and others (8 layers)"#.to_string(),
                format!("丙: \"{long}\""),
            ]
        );
    }

    #[test]
    fn notes_and_errors_in_english() {
        // 一个图层组、一个不认识的元素、一张不是 PNG 的图、两个出界的图层（其中一个混合模式不认识）、一个没名字的图层。
        let xml = r#"<image w="10" h="10" xres="300" yres="72"><stack>
            <stack><layer name="上" src="a.png" x="9" composite-op="svg:hue"/></stack>
            <layer name="下" src="a.png" y="-1"/>
            <layer src="b.png"/>
            <layer name="矢量" src="c.svg"/>
            <text/>
        </stack></image>"#;
        let png = png8(2, 2, [1, 2, 3, 255]);
        let images: [(&str, &[u8]); 3] = [("a.png", &png), ("b.png", &png), ("c.svg", b"<svg/>")];
        let (doc, notes) = in_english(|| load_parts(xml, &images)).unwrap();
        let names: Vec<&str> = doc.layers().iter().map(|l| l.props.name.as_str()).collect();
        assert_eq!(names, ["Layer 2", "下", "上", "Group"]);
        assert_eq!(
            notes,
            [
                "The file has different horizontal and vertical resolutions (300 and 72 dpi); kept the horizontal one",
                "Ignored unknown content in stack.xml: <text>",
                r#"Skipped layers whose image is not a PNG: "矢量""#,
                r#"Layers whose blend mode this program does not have were set to Normal: "上 (svg:hue)""#,
            ]
        );

        // 坏文件的错误信息。
        let error = |xml: &str, images: &[(&str, &[u8])]| in_english(|| load_parts(xml, images)).err().unwrap().to_string();
        assert_eq!(error("<image w=\"1\"", &[]), "stack.xml is not valid XML: unclosed tag");
        assert_eq!(
            error("<image><stack></image>", &[]),
            "stack.xml is not valid XML: end tag </image> does not match start tag <stack>"
        );
        assert_eq!(error("<svg/>", &[]), "Not an OpenRaster file (the root element of stack.xml is <svg>, not <image>)");
        assert_eq!(
            error(r#"<image w="10" h="10"><stack><layer src="a.png" opacity="half"/></stack></image>"#, &[("a.png", &png)]),
            "The opacity of layer 1 from the top in stack.xml is not a number: half"
        );
        assert_eq!(
            error(r#"<image w="10" h="10"><stack><layer name="画" src="data/a.png"/></stack></image>"#, &[]),
            r#"Image data/a.png of layer "画": not in the archive"#
        );
        // 保存前的检查不碰文件。
        let crowd = Document::from_layers(8, 8, [0; 3], (0..MAX_LAYERS).map(|_| Layer::new("x")).collect());
        let e = in_english(|| save(&crowd, Path::new("never-written.ora"))).err().unwrap();
        assert_eq!(e.to_string(), format!("Too many layers ({MAX_LAYERS}); this format holds at most {}", MAX_LAYERS - 1));
        // 换回中文之后还是原来的中文。
        expect_invalid(load_parts("<image w=\"1\"", &[]), "stack.xml 不是合法的 XML：标签没有结束");
    }

    #[test]
    fn rejects_files_that_are_not_openraster() {
        let tmp = Temp::new("garbage.ora");
        assert_eq!(load(&tmp.0).err().expect("文件不存在").kind(), io::ErrorKind::NotFound);
        std::fs::write(&tmp.0, b"").unwrap();
        expect_invalid(load(&tmp.0), "ZIP");
        std::fs::write(&tmp.0, "这只是一段文字，不是压缩包。".repeat(50)).unwrap();
        expect_invalid(load(&tmp.0), "ZIP");
        // 截断的压缩包。
        let good = Temp::new("good.ora");
        save(&sample_document(), &good.0).unwrap();
        let bytes = std::fs::read(&good.0).unwrap();
        for cut in [bytes.len() / 2, bytes.len() - 1, 60, 4] {
            std::fs::write(&tmp.0, &bytes[..cut]).unwrap();
            expect_invalid(load(&tmp.0), "ZIP");
        }
        // 是压缩包，但不是这种文件。
        write_zip(&tmp.0, &[("readme.txt", b"hello")]);
        expect_invalid(load(&tmp.0), "stack.xml");
        expect_invalid(load_parts("<svg/>", &[]), "根元素");
        expect_invalid(load_parts(r#"<image w="10" h="10"/>"#, &[]), "<stack>");
        // stack.xml 不是 UTF-8、大得不正常。
        let utf16: Vec<u8> = "\u{FEFF}<image/>".encode_utf16().flat_map(u16::to_le_bytes).collect();
        write_zip(&tmp.0, &[(STACK_XML, &utf16)]);
        expect_invalid(load(&tmp.0), "UTF-8");
        write_zip(&tmp.0, &[(STACK_XML, " ".repeat(MAX_XML_BYTES as usize + 1).as_bytes())]);
        expect_invalid(load(&tmp.0), "大得不正常");
    }

    #[test]
    fn rejects_bad_xml() {
        let bad = |xml: &str, keyword: &str| {
            let e = parse_xml(xml).err().unwrap_or_else(|| panic!("应当报错：{xml}"));
            assert!(e.contains(keyword), "“{xml}”的错误信息里应当有“{keyword}”：{e}");
            expect_invalid(load_parts(xml, &[]), "不是合法的 XML");
        };
        bad("", "没有根元素");
        bad("  \n", "没有根元素");
        bad("just text", "没有根元素");
        bad("<image", "标签没有结束");
        bad("<image w=\"1\"", "标签没有结束");
        bad("<image><stack></image>", "对不上");
        bad("<image></image></image>", "根元素之后");
        bad("<image/><image/>", "根元素之后");
        bad("<image/>text", "根元素之后");
        bad("<image><stack>", "就完了");
        bad("<image>", "就完了");
        bad("</image>", "多出一个结束标签");
        bad("<image w=1/>", "没有加引号");
        bad("<image w/>", "没有等号");
        bad("<image w=\"1/>", "没有结束");
        bad("<image w=\"1\" w=\"2\"/>", "出现了两次");
        bad("<image w=\"a<b\"/>", "值里有 <");
        bad("<image w=\"&nbsp;\"/>", "不认识的实体");
        bad("<image w=\"&#xD800;\"/>", "不认识的实体");
        bad("<image w=\"&amp\"/>", "实体引用没有结束");
        bad("<image =\"1\"/>", "没有名字");
        bad("< image/>", "没有名字");
        bad("<image><!-- 没写完</image>", "注释没有结束");
        bad("<image><![CDATA[ 没写完</image>", "CDATA 段没有结束");
        bad("<?xml version='1.0'<image/>", "处理指令没有结束");
        bad("<!DOCTYPE image [ <!ENTITY a \"b\"> <image/>", "DOCTYPE 没有结束");
        bad("<image></image", "没有写完");
        bad(&"<a>".repeat(MAX_XML_DEPTH + 1), "嵌套得太深");
        bad(&format!("<a>{}</a>", "<b/>".repeat(MAX_XML_ELEMENTS)), "元素太多");
        // 很深的嵌套不会把调用栈撑爆（这里只看不崩溃）。
        assert!(parse_xml(&"<a>".repeat(200_000)).is_err());

        // 这些是合法的：声明、DOCTYPE、注释、CDATA、处理指令、两种引号、各种实体、BOM、结束标签里的空白。
        let ok = "\u{FEFF}<?xml version='1.0'?>\n<!DOCTYPE image [ <!ELEMENT image ANY> ]>\n<!-- c -->\n<image a='1 &lt;&gt;&amp;&quot;&apos;' \
                  b = \"&#20013;&#x6587;\tx\"\n c=''>文字 &unknown; <![CDATA[ <stack> ]]><?pi ?><!-- <stack/> --><x:y z=\"'\"/><stack ></stack\n></image>\n<!-- 结尾 -->\n";
        let root = parse_xml(ok).unwrap();
        assert_eq!(
            (root.name.as_str(), root.attr("a"), root.attr("b"), root.attr("c")),
            ("image", Some("1 <>&\"'"), Some("中文 x"), Some(""))
        );
        let names: Vec<_> = root.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["x:y", "stack"]);
        assert_eq!((root.children[0].attr("z"), root.attr("nope")), (Some("'"), None));
        // 嵌套正好到上限是可以的。
        let deep = format!("{}{}", "<a>".repeat(MAX_XML_DEPTH), "</a>".repeat(MAX_XML_DEPTH));
        assert!(parse_xml(&deep).is_ok());
    }

    #[test]
    fn rejects_bad_stack_contents() {
        let png = png8(2, 2, [1, 2, 3, 255]);
        let with = |image_attrs: &str, layer_attrs: &str| {
            let xml = format!(r#"<image {image_attrs}><stack><layer {layer_attrs}/></stack></image>"#);
            load_parts(&xml, &[("a.png", &png)])
        };
        let size = r#"w="10" h="10""#;
        let layer = r#"name="a" src="a.png""#;
        assert!(with(size, layer).is_ok());

        expect_invalid(with(r#"w="10""#, layer), "w、h");
        expect_invalid(with(r#"w="10" h="ten""#, layer), "w、h");
        expect_invalid(with(r#"w="10.5" h="10""#, layer), "w、h");
        expect_invalid(with(r#"w="0" h="10""#, layer), "画布尺寸");
        expect_invalid(with(r#"w="10" h="-3""#, layer), "画布尺寸");
        expect_invalid(with(&format!(r#"w="{}" h="10""#, MAX_SIDE + 1), layer), "画布尺寸");
        expect_invalid(with(r#"w="10" h="99999999999999999""#, layer), "画布尺寸");
        // 最大的画布本身是合法的。
        assert_eq!(with(&format!(r#"w="{MAX_SIDE}" h="1""#), layer).unwrap().0.width, MAX_SIDE);

        expect_invalid(with(size, r#"name="a""#), "没有指定图像");
        expect_invalid(with(size, r#"name="a" src="""#), "没有指定图像");
        expect_invalid(with(size, r#"src="a.png" x="left""#), "位置 x");
        expect_invalid(with(size, r#"src="a.png" y="nan""#), "位置 y");
        expect_invalid(with(size, r#"src="a.png" opacity="half""#), "不透明度");
        expect_invalid(with(size, r#"src="a.png" opacity="nan""#), "不透明度");
        let group = r#"<image w="10" h="10"><stack><stack opacity="x"><layer src="a.png"/></stack></stack></image>"#;
        expect_invalid(load_parts(group, &[("a.png", &png)]), "图层组的不透明度");

        // 这些不算错：分辨率不对就用默认值，不透明度出界就夹回来，偏移量是小数就四舍五入、离谱就整个裁掉。
        let (doc, notes) = with(r#"w="10" h="10" xres="abc" yres="-1""#, r#"src="a.png" opacity="7" x="1.6" y=" 2 ""#).unwrap();
        assert_eq!((doc.dpi, doc.layers()[0].props.opacity, notes.len()), (72, 1.0, 0));
        assert_eq!(content_bounds(&doc.layers()[0].tiles, doc.bounds()), Rect { x0: 2, y0: 2, x1: 4, y1: 4 });
        let (doc, notes) =
            with(r#"w="10" h="10" xres="300" yres="72""#, r#"src="a.png" opacity="-1" x="9223372036854775807" y="-1e300""#)
                .unwrap();
        assert_eq!((doc.dpi, doc.layers()[0].props.opacity), (300, 0.0));
        assert!(doc.layers()[0].tiles.is_empty());
        expect_note(&notes, "分辨率不一样（300 和 72）");
        expect_note(&notes, "太远的部分已裁掉：“图层 1”");
        let (doc, _) = with(r#"w="10" h="10" yres="96.4""#, layer).unwrap();
        assert_eq!(doc.dpi, 96);

        // 图层太多。
        let many = format!(r#"<image w="1" h="1"><stack>{}</stack></image>"#, r#"<layer src="a.png"/>"#.repeat(MAX_LAYERS + 1));
        expect_invalid(load_parts(&many, &[("a.png", &png)]), "图层太多");
    }

    #[test]
    fn rejects_missing_or_broken_layer_images() {
        let xml = r#"<image w="10" h="10"><stack><layer name="画" src="data/a.png"/></stack></image>"#;
        let with_png = |png: &[u8]| load_parts(xml, &[("data/a.png", png)]);
        let good = png8(4, 4, [1, 2, 3, 255]);
        assert!(with_png(&good).is_ok());
        // 图像不在压缩包里。
        expect_invalid(load_parts(xml, &[]), "图层“画”的图像 data/a.png：不在压缩包里");
        expect_invalid(load_parts(xml, &[("data/A.png", &good)]), "不在压缩包里");
        // 截断的 PNG、被改坏的 PNG、只有文件头的 PNG。
        expect_invalid(with_png(&good[..20]), "文件头不完整");
        expect_invalid(with_png(&good[..good.len() - 20]), "图层“画”的图像");
        let mut corrupt = good.clone();
        let middle = corrupt.len() - 20;
        corrupt[middle] ^= 0x55;
        expect_invalid(with_png(&corrupt), "无法解码");
        let mut wrong_chunk = good.clone();
        wrong_chunk[12..16].copy_from_slice(b"IDAT");
        expect_invalid(with_png(&wrong_chunk), "文件头不对");
        let header_only = |w: u32, h: u32| {
            let mut out = Vec::new();
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Sixteen);
            drop(enc.write_header().unwrap());
            out
        };
        expect_invalid(with_png(&header_only(4, 4)), "无法解码");
        // 自称大得离谱的图：在分配内存之前就该被挡下。
        expect_invalid(with_png(&header_only(MAX_PNG_SIDE + 1, 4)), "尺寸不正常");
        expect_invalid(with_png(&header_only(4, 0x7FFF_FFFF)), "尺寸不正常");
        let mut zero = good.clone();
        zero[16..20].copy_from_slice(&[0; 4]);
        expect_invalid(with_png(&zero), "尺寸不正常");
        // 不是 PNG 的图层图像不算错，跳过并说明。
        let (doc, notes) = with_png(b"GIF89a....").unwrap();
        assert!(doc.layers()[0].tiles.is_empty());
        expect_note(&notes, "不是 PNG，已跳过：“画”");
        let (_, notes) = with_png(b"").unwrap();
        expect_note(&notes, "不是 PNG");
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

    /// 手工拼一张隔行扫描（Adam7）的 8 位 RGBA PNG——png 库的编码器写不出这种文件。
    /// zlib 那一层用不压缩的"存储块"，所以图像数据不能超过 65535 字节。
    fn interlaced_png(w: usize, h: usize, pixel: impl Fn(usize, usize) -> [u8; 4]) -> Vec<u8> {
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
                    raw.extend(pixel(x, y));
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
        ihdr.extend([8, 6, 0, 0, 1]); // 位深 8、RGBA、压缩方式 0、滤波方式 0、隔行扫描

        let mut out = PNG_SIGNATURE.to_vec();
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
        let source = |x: usize, y: usize| [(x * 28) as u8, (y * 40) as u8, 255, (255 - 12 * (x + y)) as u8];
        let png = interlaced_png(9, 6, source);
        let (info, data) = decode_png(&png);
        assert_eq!((info.width, info.height, data.len()), (9, 6, 9 * 6 * 4));

        let xml = r#"<image w="100" h="80"><stack><layer name="隔行" src="i.png" x="60" y="62"/></stack></image>"#;
        let (doc, notes) = load_parts(xml, &[("i.png", &png)]).unwrap();
        assert_eq!(notes, Vec::<String>::new());
        for y in 0..80 {
            for x in 0..100 {
                let inside = (60..69).contains(&x) && (62..68).contains(&y);
                let want = if inside {
                    to_premultiplied(source(x as usize - 60, y as usize - 62).map(|v| v as u16 * 257))
                } else {
                    [0; 4]
                };
                assert_eq!(pixel(&doc.layers()[0], x, y), want, "({x}, {y})");
            }
        }
    }

    #[test]
    fn saving_over_an_existing_file_replaces_it_and_leaves_no_temp_files() {
        let dir = Temp::new("dir");
        std::fs::create_dir(&dir.0).unwrap();
        let path = dir.0.join(format!("画.{EXTENSION}"));
        let listing = || {
            let mut names: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
            names.sort();
            names
        };

        save(&Document::new(64, 48), &path).unwrap();
        assert_eq!(load(&path).unwrap().0.width, 64);
        save(&sample_document(), &path).unwrap();
        let (back, _) = load(&path).unwrap();
        assert_eq!((back.width, back.height, back.layers().len()), (200, 130, 8));
        assert_eq!(listing(), [path.file_name().unwrap()]);

        // 存不成的时候报错，目标原样不动，临时文件也收拾掉。这里的目标是个目录，改名顶替不了它。
        let blocked = dir.0.join("挡路的目录");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("里面有东西"), b"x").unwrap();
        assert!(save(&back, &blocked).is_err());
        assert!(blocked.join("里面有东西").is_file());
        assert_eq!(listing().len(), 2);
        assert!(save(&back, &dir.0.join("没有这个目录").join("a.ora")).is_err());
        assert_eq!(listing().len(), 2);
        // 尺寸不对、图层多到自己都不肯读回来的文档不写。
        let mut bad = Document::new(8, 8);
        bad.height = MAX_SIDE + 1;
        assert_eq!(save(&bad, &dir.0.join("bad.ora")).err().unwrap().kind(), io::ErrorKind::InvalidInput);
        let crowd = Document::from_layers(8, 8, [0; 3], (0..MAX_LAYERS).map(|i| Layer::new(&i.to_string())).collect());
        assert_eq!(save(&crowd, &dir.0.join("crowd.ora")).err().unwrap().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(listing().len(), 2);
    }

    /// 把一份好的 stack.xml 随机改坏几千次：结果可以是报错，也可以是读成了别的样子，但不能崩溃。
    #[test]
    fn mangled_stack_xml_never_panics() {
        let good = Temp::new("fuzz.ora");
        save(&sample_document(), &good.0).unwrap();
        let xml = read_entry(&good.0, STACK_XML);
        let mut rng = Rng(0xC0FF_EE11);
        let mut notes = Notes::default();
        for round in 0..4000 {
            let mut bytes = xml.clone();
            for _ in 0..1 + round % 4 {
                let at = rng.next() as usize % bytes.len();
                match rng.next() % 4 {
                    0 => bytes[at] = b"<>/\"'=&; \n"[rng.next() as usize % 10],
                    1 => bytes[at] = rng.next() as u8,
                    2 => drop(bytes.remove(at)),
                    _ => bytes.truncate(at),
                }
                if bytes.is_empty() {
                    break;
                }
            }
            if let Ok(text) = String::from_utf8(bytes) {
                let _ = parse_stack(&text, &mut notes);
            }
        }
    }

    /// 和 GIMP 互相读写的核对用的两个辅助"测试"，平时不跑。设好环境变量 `ZHIBI_INTEROP_DIR` 再跑：
    /// `cargo test -p zhibi-core interop -- --ignored --nocapture`
    /// 第一个把示例文档写成 `ours.ora`，同时把文档的内容另存一份（`ours.expected.*`）供对照；
    /// 第二个读目录里别的程序写的 `.ora` 文件，把读到的内容存成 `<文件名>.loaded.*`。
    #[test]
    #[ignore]
    fn interop_write_sample() {
        let Some(dir) = interop_dir() else { return };
        let doc = sample_document();
        save(&doc, &dir.join("ours.ora")).unwrap();
        dump_for_interop(&Ok((doc, Vec::new())), &dir.join("ours.expected"));
        // 有文件夹、蒙版、填充图层的那份（和 PSD 的测试同一个文档），合成的样子存成 `ours-tree.ora.png`。
        let tree = crate::psd::tests::tree_document();
        save(&tree, &dir.join("ours-tree.ora")).unwrap();
        tree.export_png(&dir.join("ours-tree.ora.png")).unwrap();
    }

    #[test]
    #[ignore]
    fn interop_read_foreign() {
        interop_load_all(EXTENSION, load);
    }
}
