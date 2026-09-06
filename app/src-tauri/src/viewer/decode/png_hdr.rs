//! HDR PNG（16bit PQ + cICP BT.2020）解码查看
//!
//! 浏览器/WebView2 不识别 PNG cICP chunk，16bit PQ 值会被当作 sRGB 直显
//! （画面极暗偏灰）。此处专门识别 cICP(PQ) PNG：PQ 解码 → 线性光
//! （BT.2020）→ 色域转换 + 色调映射（与截图 SDR 输出同一管线）→
//! SDR 临时 PNG 预览。原始 HDR 数据不受影响。

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::capture::hdr_pipeline::to_sdr;
use crate::capture::CapturedTexture;
use crate::color::{pq_eotf, PixelFormat};

use super::exr::f32_to_f16;
use super::tiff::write_temp_png;
use super::HdrSource;

const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// 是否为 HDR PNG（含 cICP chunk 且传递函数为 PQ=12）
pub fn is_hdr_png(path: &Path) -> bool {
    read_cicp(path)
        .map(|c| c[1] == 12) // [primaries, transfer, matrix, full_range]
        .unwrap_or(false)
}

/// 读取 cICP（PQ=12 判 HDR；primaries 9=BT.2020 / 11=P3 / 1=BT.709）
fn read_cicp(path: &Path) -> Option<[u8; 4]> {
    // 元数据 chunk（IHDR/cICP 等）在 IDAT 之前，读头部 64KB 足够覆盖
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    find_cicp(&buf)
}

/// cICP primaries → src→BT.709 色域矩阵（P3：负分量保留给 DWM 原生重现）
fn cicp_gamut(path: &Path) -> crate::color::Mat3 {
    use crate::color::{gamut_matrix, Mat3, BT2020, BT709, DCI_P3};
    match read_cicp(path).map(|c| c[0]) {
        Some(11) => gamut_matrix(DCI_P3, BT709), // P3
        Some(9) => gamut_matrix(BT2020, BT709),  // BT.2020
        Some(1) => Mat3::IDENTITY,               // BT.709/sRGB
        _ => crate::color::bt2020_to_bt709(),    // 缺省按 2020（本工具输出）
    }
}

/// 在 PNG 字节流中查找 cICP chunk（遍历至 IDAT/IEND 为止）
fn find_cicp(buf: &[u8]) -> Option<[u8; 4]> {
    if buf.len() < 8 || buf[..8] != PNG_SIG {
        return None;
    }
    let mut pos = 8usize;
    while pos + 8 <= buf.len() {
        let len = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        let ctype = &buf[pos + 4..pos + 8];
        if ctype == b"IDAT" || ctype == b"IEND" {
            break;
        }
        let data_start = pos + 8;
        if data_start + 4 > buf.len() {
            break;
        }
        if ctype == b"cICP" {
            return Some([
                buf[data_start],
                buf[data_start + 1],
                buf[data_start + 2],
                buf[data_start + 3],
            ]);
        }
        // 跳过 chunk data + CRC（防 len 异常越界）
        pos = data_start.saturating_add(len).saturating_add(4);
    }
    None
}

/// 解码 HDR PNG → 色调映射 → 临时 PNG，返回 (临时 PNG 路径, 宽, 高)
pub fn decode_to_temp_png(path: &Path) -> std::result::Result<(PathBuf, u32, u32), String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| format!("打开文件失败: {}", e))?
        .with_guessed_format()
        .map_err(|e| format!("识别格式失败: {}", e))?
        .decode()
        .map_err(|e| format!("HDR PNG 解码失败: {}", e))?;
    let rgba16 = img.to_rgba16();
    let (w, h) = (rgba16.width(), rgba16.height());
    let gamut = cicp_gamut(path);
    cache_hdr_source_rgba16(path, &rgba16, gamut);
    let sdr = tonemap_to_sdr(&rgba16, gamut);
    let out =
        image::RgbaImage::from_raw(w, h, sdr).ok_or_else(|| "HDR PNG 数据异常".to_string())?;
    let temp = write_temp_png(&image::DynamicImage::ImageRgba8(out), path)?;
    log::info!("HDR PNG 已解码并色调映射: {} ({}x{})", path.display(), w, h);
    Ok((temp, w, h))
}

/// HDR PNG → DynamicImage（缩略图用，同色调映射）
pub fn decode_image(path: &Path) -> std::result::Result<image::DynamicImage, String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| format!("打开文件失败: {}", e))?
        .with_guessed_format()
        .map_err(|e| format!("识别格式失败: {}", e))?
        .decode()
        .map_err(|e| format!("HDR PNG 解码失败: {}", e))?;
    let rgba16 = img.to_rgba16();
    let (w, h) = (rgba16.width(), rgba16.height());
    let gamut = cicp_gamut(path);
    cache_hdr_source_rgba16(path, &rgba16, gamut);
    let sdr = tonemap_to_sdr(&rgba16, gamut);
    image::RgbaImage::from_raw(w, h, sdr)
        .map(image::DynamicImage::ImageRgba8)
        .ok_or_else(|| "HDR PNG 数据异常".to_string())
}

/// 16bit PQ RGBA → scRGB f16 → 缓存为 HdrSource（重渲数据源；行级并行）
fn cache_hdr_source_rgba16(
    path: &Path,
    rgba16: &image::ImageBuffer<image::Rgba<u16>, Vec<u16>>,
    gamut: crate::color::Mat3,
) {
    let w = rgba16.width() as usize;
    let h = rgba16.height() as usize;
    let raw = rgba16.as_raw();
    let mut data = vec![0u8; w * h * 8];
    let row_in = w * 4;
    super::par_rows_mut(&mut data, w * 8, |out_row, y| {
        let in_row = &raw[y * row_in..(y + 1) * row_in];
        for (px, ins) in out_row.chunks_mut(8).zip(in_row.chunks_exact(4)) {
            let r = pq_eotf(ins[0] as f32 / 65535.0) * 10000.0;
            let g = pq_eotf(ins[1] as f32 / 65535.0) * 10000.0;
            let b = pq_eotf(ins[2] as f32 / 65535.0) * 10000.0;
            let (r, g, b) = gamut.apply(r, g, b);
            px[0..2].copy_from_slice(&f32_to_f16(r / 80.0).to_le_bytes());
            px[2..4].copy_from_slice(&f32_to_f16(g / 80.0).to_le_bytes());
            px[4..6].copy_from_slice(&f32_to_f16(b / 80.0).to_le_bytes());
            px[6..8].copy_from_slice(&0x3C00u16.to_le_bytes());
        }
    });
    super::cache_hdr_source(
        path,
        HdrSource {
            width: w as u32,
            height: h as u32,
            data,
        },
    );
}

/// 16bit PQ RGBA → SDR RGBA8（PQ 解码 → 线性光 → 色域转换 → 色调映射）
///
/// JXL（16bit PQ）解码复用同一管线（decode 模块内可见）。
/// 同时把 scRGB 纹理缓存为 HdrSource（「亮度调节」重渲数据源）。
/// PQ→scRGB 段行级并行（4M 像素 CPU 大头）。
pub(super) fn tonemap_to_sdr(
    rgba16: &image::ImageBuffer<image::Rgba<u16>, Vec<u16>>,
    gamut: crate::color::Mat3,
) -> Vec<u8> {
    let w = rgba16.width() as usize;
    let h = rgba16.height() as usize;
    let raw = rgba16.as_raw();

    // PQ 解码：u16 → [0,1] → 绝对 nits（0..1 = 0..10000）→ src 原色→BT.709
    // → scRGB（1.0 = 80 nit 绝对语义），复用 to_sdr 色调映射管线
    let mut data = vec![0u8; w * h * 8]; // 4 × f16 = 8 bytes/px
    let row_in = w * 4;
    super::par_rows_mut(&mut data, w * 8, |out_row, y| {
        let in_row = &raw[y * row_in..(y + 1) * row_in];
        for (px, ins) in out_row.chunks_mut(8).zip(in_row.chunks_exact(4)) {
            let r = pq_eotf(ins[0] as f32 / 65535.0) * 10000.0;
            let g = pq_eotf(ins[1] as f32 / 65535.0) * 10000.0;
            let b = pq_eotf(ins[2] as f32 / 65535.0) * 10000.0;
            let (r, g, b) = gamut.apply(r, g, b);
            // nits → scRGB（1.0 = 80 nit）
            px[0..2].copy_from_slice(&f32_to_f16(r / 80.0).to_le_bytes());
            px[2..4].copy_from_slice(&f32_to_f16(g / 80.0).to_le_bytes());
            px[4..6].copy_from_slice(&f32_to_f16(b / 80.0).to_le_bytes());
            px[6..8].copy_from_slice(&0x3C00u16.to_le_bytes()); // A = 1.0
        }
    });
    let captured = CapturedTexture {
        width: w as u32,
        height: h as u32,
        format: PixelFormat::R16g16b16a16Float,
        data: data.clone(),
        row_pitch: w * 8,
        via_gdi: false,
    };

    // 统一参数入口：归一化基准用【当前显示器实际 SDR 白】——
    // PQ 文件存绝对 nits，屏幕白（如 200 nits）应映射到 y_rel=1.0（SDR 白），
    // 与截图时的 SDR 预览观感一致。旧实现硬编码 80：200nits 白被当成 2.5×
    // SDR 白 → y_rel 虚高 2.5 倍 → 预览整体过曝
    let config = crate::config::Config::load();
    let params = config.active_tonemap_params(crate::capture::monitor::get_sdr_white_level_nits());
    to_sdr(&captured, &params).rgba
}
