//! PSD 解码（Tier 3）：psd crate（纯 Rust，仅支持 8bit）
//!
//! 优先 flatten_layers_rgba 图层合成（自顶向下 alpha 混合；无图层时 crate 内部
//! 自动回退合成图像数据），合成结果 RGBA8 → PNG → 写临时文件。

use std::path::{Path, PathBuf};

use image::{DynamicImage, RgbaImage};

use super::tiff::write_temp_png;

/// 解码 PSD → 内存图像（供缩略图复用，不落盘）
pub fn decode_image(path: &Path) -> Result<DynamicImage, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
    let psd = psd::Psd::from_bytes(&bytes).map_err(|e| format!("PSD 解析失败: {}", e))?;

    let (w, h) = (psd.width(), psd.height());
    // 图层合成；合成失败（如 16bit 深度）回退整图数据 rgba()
    let rgba = match psd.flatten_layers_rgba(&|_| true) {
        Ok(px) => px,
        Err(_) => psd.rgba(),
    };

    // 合成像素 → RGBA 图像缓冲
    let buf =
        RgbaImage::from_raw(w, h, rgba).ok_or_else(|| "PSD 像素数据与画布尺寸不符".to_string())?;
    Ok(DynamicImage::ImageRgba8(buf))
}

/// 解码 PSD 文件 → 临时 PNG，返回 (临时 PNG 路径, 宽, 高)
pub fn decode_to_temp_png(path: &Path) -> Result<(PathBuf, u32, u32), String> {
    let img = decode_image(path)?;
    let (w, h) = (img.width(), img.height());
    let temp = write_temp_png(&img, path)?;
    Ok((temp, w, h))
}
