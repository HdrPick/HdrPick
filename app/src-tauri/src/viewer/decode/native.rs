//! 浏览器原生格式（Tier 1）：读字节头判定格式与宽高，不做完整解码
//!
//! 覆盖：PNG（IHDR）/ JPEG（SOF 段）/ GIF（逻辑屏幕描述 + block 遍历计帧数）/
//! BMP（DIB 头）/ WebP（VP8/VP8L/VP8X chunk）/ AVIF（ftyp 判定，尺寸由前端 img onload 兜底）。
//! 另提供 PSD / ICO 的头部快速尺寸解析（供目录列表扫描用，避免完整解码）。

use std::path::Path;

/// 原生格式探测结果（format 为规范化小写格式名）
#[derive(Debug, Clone)]
pub struct NativeInfo {
    /// 规范化格式名：png / jpeg / gif / bmp / webp / avif
    pub format: String,
    /// 宽（像素）；无法解析时为 0（前端 img onload 兜底）
    pub width: u32,
    /// 高（像素）；无法解析时为 0
    pub height: u32,
    /// 帧数（GIF 动画统计 Image Descriptor 个数；其余恒为 1）
    pub frame_count: u32,
}

/// 读文件头部指定字节数（文件不足时返回实际读取长度）
fn read_header(path: &Path, len: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {}", e))?;
    let mut buf = vec![0u8; len];
    let mut read = 0;
    while read < len {
        match f.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) => return Err(format!("读取文件失败: {}", e)),
        }
    }
    buf.truncate(read);
    Ok(buf)
}

/// 大端 u16
fn be16(b: &[u8], off: usize) -> u32 {
    ((b[off] as u32) << 8) | b[off + 1] as u32
}

/// 大端 u32
fn be32(b: &[u8], off: usize) -> u32 {
    ((b[off] as u32) << 24)
        | ((b[off + 1] as u32) << 16)
        | ((b[off + 2] as u32) << 8)
        | b[off + 3] as u32
}

/// 小端 u16
fn le16(b: &[u8], off: usize) -> u32 {
    ((b[off + 1] as u32) << 8) | b[off] as u32
}

/// 小端 u32
fn le32(b: &[u8], off: usize) -> u32 {
    ((b[off + 3] as u32) << 24)
        | ((b[off + 2] as u32) << 16)
        | ((b[off + 1] as u32) << 8)
        | b[off] as u32
}

/// 小端 24 位（WebP VP8X 画布尺寸用）
fn le24(b: &[u8], off: usize) -> u32 {
    ((b[off + 2] as u32) << 16) | ((b[off + 1] as u32) << 8) | b[off] as u32
}

/// PNG：89 50 4E 47 0D 0A 1A 0A + IHDR（偏移 16 起宽高，大端）
fn probe_png(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 24 || &h[..8] != b"\x89PNG\r\n\x1a\n" || &h[12..16] != b"IHDR" {
        return None;
    }
    Some((be32(h, 16), be32(h, 20)))
}

/// JPEG：FF D8 开头，扫描 SOF0-SOF15 段读取宽高（遇 SOS 停止，其后为熵编码数据）
fn scan_jpeg_sof(h: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    while i + 9 < h.len() {
        // 寻找标记前导 0xFF（填充字节 0xFF 可连续出现）
        if h[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = h[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        // SOI/EOI 无长度字段；SOS 之后是压缩数据，无法继续按段遍历
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if marker == 0xDA {
            return None; // 未在头 32 字节内找到 SOF（罕见），交由前端兜底
        }
        if i + 4 > h.len() {
            break;
        }
        let seg_len = be16(h, i + 2) as usize;
        // SOF0-SOF15（跳过 C4=DHT / C8=JPG / CC=DAC）：段内 精度(1) 高(2) 宽(2)
        if (0xC0..=0xCF).contains(&marker)
            && marker != 0xC4
            && marker != 0xC8
            && marker != 0xCC
            && i + 9 <= h.len()
        {
            return Some((be16(h, i + 7), be16(h, i + 5)));
        }
        i += 2 + seg_len;
    }
    None
}

/// JPEG 判定 + 尺寸扫描（SOF 可能位于长 EXIF 之后：64 → 1KB 逐级加大读取窗口）
fn probe_jpeg(path: &Path, header: &[u8]) -> Option<(u32, u32)> {
    if header.len() < 4 || header[0] != 0xFF || header[1] != 0xD8 {
        return None;
    }
    if let Some(dims) = scan_jpeg_sof(header) {
        return Some(dims);
    }
    // 64 字节内未找到 SOF（EXIF 缩略图等长前置段）：放大窗口重试
    for win in [256usize, 1024] {
        if let Ok(big) = read_header(path, win) {
            if let Some(dims) = scan_jpeg_sof(&big) {
                return Some(dims);
            }
        }
    }
    // 仍未找到：返回零尺寸由前端 img onload 兜底
    Some((0, 0))
}

/// GIF：GIF87a/GIF89a + 逻辑屏幕描述（偏移 6/8 小端宽高）
fn probe_gif_header(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 10 || (&h[..6] != b"GIF87a" && &h[..6] != b"GIF89a") {
        return None;
    }
    Some((le16(h, 6), le16(h, 8)))
}

/// GIF 帧数：遍历数据块统计 Image Descriptor（0x2C）个数
///
/// 块结构：扩展块 0x21+label+子块链；图像描述 0x2C+10 字节+[局部色表]+LZW 子块链；
/// 结束符 0x3B。子块链以长度 0 的子块收尾。
fn gif_frame_count(data: &[u8]) -> u32 {
    if data.len() < 13 {
        return 1;
    }
    let mut i = 13usize; // 跳过 头(6) + 逻辑屏幕描述(7)
    let mut frames = 0u32;
    // 全局色表长度（逻辑屏幕描述 packed 位 7 置位时存在：3 × 2^(n+1) 字节）
    let packed = data[10];
    if packed & 0x80 != 0 {
        i += 3usize * (1 << ((packed & 0x07) + 1));
    }
    while i < data.len() {
        match data[i] {
            0x21 => {
                // 扩展块：label(1) + 子块链
                i += 2;
                i = skip_sub_blocks(data, i);
            }
            0x2C => {
                // 图像描述：left(2) top(2) w(2) h(2) packed(1)
                frames += 1;
                if i + 10 > data.len() {
                    break;
                }
                let lp = data[i + 9];
                i += 10;
                if lp & 0x80 != 0 {
                    // 局部色表：3 × 2^(n+1) 字节
                    i += 3usize * (1 << ((lp & 0x07) + 1));
                }
                // LZW 最小码长(1) + 子块链
                i += 1;
                i = skip_sub_blocks(data, i);
            }
            0x3B => break, // 结束符
            _ => break,    // 非法块（或截断），停止计数
        }
    }
    frames.max(1)
}

/// 跳过 GIF 子块链（每子块：长度 1 字节 + 数据；长度 0 表示链结束），返回新偏移
fn skip_sub_blocks(data: &[u8], mut i: usize) -> usize {
    while i < data.len() {
        let len = data[i] as usize;
        i += 1;
        if len == 0 {
            break;
        }
        i += len;
    }
    i.min(data.len())
}

/// BMP：BM + DIB 头（偏移 18/22 小端宽高，负值表示自底向上，取绝对值）
fn probe_bmp(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 26 || &h[..2] != b"BM" {
        return None;
    }
    let w = le32(h, 18) as i32;
    let hh = le32(h, 22) as i32;
    Some((w.unsigned_abs(), hh.unsigned_abs()))
}

/// WebP：RIFF....WEBP + VP8X（扩展）/ VP8（有损）/ VP8L（无损）chunk
fn probe_webp(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 16 || &h[..4] != b"RIFF" || &h[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &h[12..];
    if chunk.len() < 4 {
        return Some((0, 0));
    }
    match &chunk[..4] {
        // 扩展格式：flags(1) + reserved(3) + 画布宽-1(3 LE) + 画布高-1(3 LE)
        b"VP8X" => {
            if chunk.len() >= 14 {
                Some((le24(chunk, 8) + 1, le24(chunk, 11) + 1))
            } else {
                Some((0, 0))
            }
        }
        // 无损：signature(1) + 14 位宽 + 14 位高 + alpha + version（4 字节小端位域）
        b"VP8L" => {
            if chunk.len() >= 9 {
                let bits = le32(chunk, 5);
                Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            } else {
                Some((0, 0))
            }
        }
        // 有损：帧标签(3) + 同步码(3) + 14 位宽 + 14 位高
        b"VP8 " => {
            if chunk.len() >= 10 {
                Some((le16(chunk, 6) & 0x3FFF, le16(chunk, 8) & 0x3FFF))
            } else {
                Some((0, 0))
            }
        }
        _ => Some((0, 0)), // ANIM 等未知 chunk：尺寸交由前端兜底
    }
}

/// AVIF：ISO BMFF，偏移 4 处 ftyp box，brands 含 avif/avis 即判定
/// 宽高在 ispe box 中，解析成本高 → 返回零尺寸由前端 img onload 兜底
fn probe_avif(h: &[u8]) -> bool {
    h.len() >= 12 && &h[4..8] == b"ftyp" && {
        // major brand(4) + minor version(4) + compatible brands...
        let rest = &h[8..];
        if &rest[..4] == b"avif" || &rest[..4] == b"avis" {
            return true;
        }
        rest.len() >= 8 && rest[8..].chunks(4).any(|b| b == b"avif" || b == b"avis")
    }
}

/// PSD 头部快速尺寸（8BPS 签名 + version + reserved + channels + height + width）
/// 供目录列表扫描用，不引入完整解码
pub fn probe_psd_dimensions(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 26 || &h[..4] != b"8BPS" {
        return None;
    }
    Some((be32(h, 18), be32(h, 14)))
}

/// ICO 头部快速尺寸（00 00 01 00 + count + 目录项：width(1)/height(1)，0 表示 256）
pub fn probe_ico_dimensions(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 10 || h[0] != 0 || h[1] != 0 || h[2] != 1 || h[3] != 0 {
        return None;
    }
    let w = if h[6] == 0 { 256 } else { h[6] as u32 };
    let hh = if h[7] == 0 { 256 } else { h[7] as u32 };
    Some((w, hh))
}

/// 探测文件格式与尺寸（按字节头判定，与扩展名无关）
///
/// - GIF 需遍历全文件统计帧数（只遍历块结构不解码像素）
/// - AVIF / 无法解析的头 → 尺寸 0，由前端 img onload 用 naturalWidth/Height 兜底
/// - 完全无法识别的格式 → Err
pub fn probe(path: &Path) -> Result<NativeInfo, String> {
    // 先读 32 字节判定格式与静态尺寸（JPEG SOF 可能靠后，读 64 字节更稳妥）
    let header = read_header(path, 64)?;

    if let Some((w, h)) = probe_png(&header) {
        return Ok(NativeInfo {
            format: "png".into(),
            width: w,
            height: h,
            frame_count: 1,
        });
    }
    if let Some((w, h)) = probe_jpeg(path, &header) {
        return Ok(NativeInfo {
            format: "jpeg".into(),
            width: w,
            height: h,
            frame_count: 1,
        });
    }
    if let Some((w, h)) = probe_gif_header(&header) {
        // 帧数需要遍历完整文件（块结构遍历，不解码像素）
        let data = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
        let frames = gif_frame_count(&data);
        return Ok(NativeInfo {
            format: "gif".into(),
            width: w,
            height: h,
            frame_count: frames,
        });
    }
    if let Some((w, h)) = probe_bmp(&header) {
        return Ok(NativeInfo {
            format: "bmp".into(),
            width: w,
            height: h,
            frame_count: 1,
        });
    }
    if let Some((w, h)) = probe_webp(&header) {
        return Ok(NativeInfo {
            format: "webp".into(),
            width: w,
            height: h,
            frame_count: 1,
        });
    }
    if probe_avif(&header) {
        return Ok(NativeInfo {
            format: "avif".into(),
            width: 0,
            height: 0,
            frame_count: 1,
        });
    }

    Err("无法识别的图片格式".to_string())
}
