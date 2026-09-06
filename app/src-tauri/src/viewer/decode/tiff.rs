//! TIFF 解码（Tier 2）：image crate `tiff` feature
//!
//! 解码 → RGBA8 → PNG 编码 → 写入 %TEMP%\jietu-hdr\decode_<hash>.png，
//! 前端用临时 PNG 路径加载（convertFileSrc）。

use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat};

use super::temp_png_path;

/// 解码 TIFF 文件 → 临时 PNG，返回 (临时 PNG 路径, 宽, 高)
pub fn decode_to_temp_png(path: &Path) -> Result<(PathBuf, u32, u32), String> {
    // image crate 按 content 探测（TIFF 头 II*/MM*），失败时报错给前端
    let img = image::ImageReader::open(path)
        .map_err(|e| format!("打开文件失败: {}", e))?
        .with_guessed_format()
        .map_err(|e| format!("识别格式失败: {}", e))?
        .decode()
        .map_err(|e| format!("TIFF 解码失败: {}", e))?;

    let (w, h) = (img.width(), img.height());
    let temp = write_temp_png(&img, path)?;
    Ok((temp, w, h))
}

/// RGBA 图像编码为 PNG 并写入 %TEMP%\jietu-hdr\decode_<hash>.png（同源路径复用同一临时文件）
pub fn write_temp_png(img: &DynamicImage, src_path: &Path) -> Result<PathBuf, String> {
    let temp = temp_png_path(src_path);
    let file = std::fs::File::create(&temp).map_err(|e| format!("创建临时文件失败: {}", e))?;
    let mut w = std::io::BufWriter::new(file);
    img.write_to(&mut w, ImageFormat::Png)
        .map_err(|e| format!("PNG 编码失败: {}", e))?;
    log::debug!("已解码到临时 PNG: {}", temp.display());
    Ok(temp)
}
