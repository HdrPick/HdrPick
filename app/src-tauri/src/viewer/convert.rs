//! 转格式（另存为）：解码原格式 → 按目标格式编码 → 写入目标路径
//!
//! 支持目标格式：png / jpeg / webp。
//! - jpeg：质量 1-100（JpegEncoder new_with_quality）
//! - webp：image crate 0.25 仅支持无损编码（quality 参数不生效）
//! - **HDR 源（EXR / HDR PNG / JXL / JXR）**：走统一解码入口（decode::decode_image），
//!   用设置菜单当前激活预设重新色调映射后输出——切换预设再另存即得新效果

use std::io::BufWriter;
use std::path::Path;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageFormat};

use super::decode;

/// 转换图片格式：src → dst（format: "png" | "jpeg" | "webp"；quality: 1-100，仅 jpeg 生效）
pub fn convert(src: &Path, dst: &Path, format: &str, quality: u32) -> Result<(), String> {
    // 质量钳制到 1-100（前端滑块传 90）
    let q = quality.clamp(1, 100) as u8;

    // 解码源图（HDR 变体用当前激活预设色调映射为 SDR）
    let img = decode::decode_image(src)?;

    // 编码写入目标文件
    let file = std::fs::File::create(dst).map_err(|e| format!("创建目标文件失败: {}", e))?;
    let mut w = BufWriter::new(file);
    match format.to_lowercase().as_str() {
        "png" => img
            .write_to(&mut w, ImageFormat::Png)
            .map_err(|e| format!("PNG 编码失败: {}", e))?,
        "jpeg" | "jpg" => {
            // JPEG 不支持透明通道 → 转 RGB8
            let rgb = img.to_rgb8();
            JpegEncoder::new_with_quality(&mut w, q)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    ExtendedColorType::Rgb8,
                )
                .map_err(|e| format!("JPEG 编码失败: {}", e))?
        }
        "webp" => {
            // image crate 0.25 WebP 仅无损（VP8L），quality 不生效
            let rgba = img.to_rgba8();
            WebPEncoder::new_lossless(&mut w)
                .write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    ExtendedColorType::Rgba8,
                )
                .map_err(|e| format!("WebP 编码失败: {}", e))?
        }
        other => return Err(format!("不支持的目标格式: {}", other)),
    }
    // 显式 flush 确保落盘（BufWriter drop 时错误会被吞掉）
    use std::io::Write;
    w.flush().map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(())
}
