//! EXR 解码（HDR 截图查看）
//!
//! 读取 32bit 浮点 RGBA（线性光 scRGB）→ 复用截图相同的 HDR→SDR
//! 色调映射管线（BT.2390 等）→ 临时 PNG 供前端显示。
//! 预览为 SDR；原始 HDR 数据仍在 EXR 文件中，另存/导出不受影响。

use std::path::{Path, PathBuf};

use exr::image::pixel_vec::PixelVec;
use exr::prelude::*;

use crate::capture::hdr_pipeline::{to_sdr, HdrToSdrParams};
use crate::capture::CapturedTexture;
use crate::color::PixelFormat;

use super::tiff::write_temp_png;

/// 解码 EXR → 色调映射 → 临时 PNG，返回 (临时 PNG 路径, 宽, 高)
pub fn decode_to_temp_png(path: &Path) -> std::result::Result<(PathBuf, u32, u32), String> {
    let (w, h, px) = read_rgba(path)?;
    let (w, h) = (w as u32, h as u32);

    // f32 线性光 → f16 scRGB 纹理，复用 to_sdr 色调映射管线
    let mut data = vec![0u8; (w * h) as usize * 8];
    for (i, (r, g, b, _)) in px.iter().enumerate() {
        let off = i * 8;
        data[off..off + 2].copy_from_slice(&f32_to_f16(*r).to_le_bytes());
        data[off + 2..off + 4].copy_from_slice(&f32_to_f16(*g).to_le_bytes());
        data[off + 4..off + 6].copy_from_slice(&f32_to_f16(*b).to_le_bytes());
        data[off + 6..off + 8].copy_from_slice(&f32_to_f16(1.0).to_le_bytes());
    }
    // 缓存 scRGB 纹理为 HdrSource（「亮度调节」重渲数据源）
    super::cache_hdr_source(
        path,
        super::HdrSource {
            width: w,
            height: h,
            data: data.clone(),
        },
    );
    let captured = CapturedTexture {
        width: w,
        height: h,
        format: PixelFormat::R16g16b16a16Float,
        data,
        row_pitch: w as usize * 8,
        via_gdi: false,
    };
    let sdr = to_sdr(&captured, &sdr_params());

    let img =
        image::RgbaImage::from_raw(w, h, sdr.rgba).ok_or_else(|| "EXR 数据异常".to_string())?;
    let temp = write_temp_png(&image::DynamicImage::ImageRgba8(img), path)?;
    log::info!("EXR 已解码并色调映射: {} ({}x{})", path.display(), w, h);
    Ok((temp, w, h))
}

/// EXR → DynamicImage（缩略图用）：scRGB 1.0 为白直接 sRGB 映射（200px 内差异不可见）
pub fn decode_image(path: &Path) -> std::result::Result<image::DynamicImage, String> {
    let (w, h, px) = read_rgba(path)?;
    let mut rgba = vec![0u8; px.len() * 4];
    for (i, (r, g, b, _)) in px.iter().enumerate() {
        let off = i * 4;
        rgba[off] = crate::color::linear_to_srgb8(r.clamp(0.0, 1.0));
        rgba[off + 1] = crate::color::linear_to_srgb8(g.clamp(0.0, 1.0));
        rgba[off + 2] = crate::color::linear_to_srgb8(b.clamp(0.0, 1.0));
        rgba[off + 3] = 255;
    }
    image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        .map(image::DynamicImage::ImageRgba8)
        .ok_or_else(|| "EXR 数据异常".to_string())
}

/// 读取 EXR 为 RGBA f32 像素（线性光），返回 (宽, 高, 像素)
fn read_rgba(
    path: &Path,
) -> std::result::Result<(usize, usize, Vec<(f32, f32, f32, f32)>), String> {
    let image: RgbaImage<PixelVec<(f32, f32, f32, f32)>> = read_first_rgba_layer_from_file(
        path,
        PixelVec::<(f32, f32, f32, f32)>::constructor,
        PixelVec::<(f32, f32, f32, f32)>::set_pixel,
    )
    .map_err(|e| format!("EXR 解码失败: {}", e))?;
    let Vec2(w, h) = image.layer_data.size;
    let pixels: PixelVec<(f32, f32, f32, f32)> = image.layer_data.channel_data.pixels;
    Ok((w, h, pixels.pixels))
}

/// 色调映射参数：与截图输出一致（统一走预设系统；归一化基准用当前
/// 显示器实际 SDR 白——EXR 存绝对 scRGB，屏幕白应映射 y_rel=1.0，
/// 旧硬编码 80 会使 200nits 屏的图过曝 2.5 倍）
fn sdr_params() -> HdrToSdrParams {
    let config = crate::config::Config::load();
    config.active_tonemap_params(crate::capture::monitor::get_sdr_white_level_nits())
}

/// f32 → f16（半精度）位转换（EXR/HDR PNG 解码 → scRGB f16 纹理用）
pub fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    let mant = (bits & 0x007F_FFFF) >> 13;
    if v.is_nan() {
        return sign | 0x7C00 | 0x0200;
    }
    if exp >= 0x1F {
        return sign | 0x7C00; // 溢出 → 无穷
    }
    if exp <= 0 {
        return sign; // 欠溢出 → 0（截图数据不涉及极小值）
    }
    sign | ((exp as u16) << 10) | (mant as u16)
}
