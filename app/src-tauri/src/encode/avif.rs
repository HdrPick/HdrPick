//! AVIF 编码器
//!
//! 使用 `ravif`（纯 Rust，基于 rav1e）实现 8bit SDR AVIF。
//! ravif 限制：仅支持 8bit 输出。HDR 数据会先色调映射为 SDR 再编码。

use std::path::Path;

use ravif::{EncodedImage, Encoder, Img, RGBA8};

use crate::capture::hdr_pipeline::{HdrToSdrParams, SdrImage};
use crate::capture::{to_sdr, CapturedTexture};

/// 保存 SDR AVIF（8bit RGBA）
pub fn save_avif_sdr(image: &SdrImage, path: &Path) -> anyhow::Result<()> {
    let width = image.width as usize;
    let height = image.height as usize;

    // ravif 的 Img 借用 &[RGBA8]
    let pixels: &[RGBA8] = unsafe {
        std::slice::from_raw_parts(image.rgba.as_ptr() as *const RGBA8, image.rgba.len() / 4)
    };
    let img = Img::new(pixels, width, height);

    let encoder = Encoder::new()
        .with_quality(80.0)
        .with_speed(4)
        .with_alpha_quality(80.0);

    let result: EncodedImage = encoder
        .encode_rgba(img)
        .map_err(|e| anyhow::anyhow!("AVIF 编码失败: {}", e))?;

    let file_size = result.avif_file.len();
    std::fs::write(path, result.avif_file)?;
    log::info!("AVIF 已保存: {} ({}KB)", path.display(), file_size / 1024);
    Ok(())
}

/// 保存 HDR AVIF（先色调映射为 SDR，再编码 8bit）
pub fn save_avif_hdr(
    captured: &CapturedTexture,
    path: &Path,
    params: &HdrToSdrParams,
) -> anyhow::Result<()> {
    log::info!("AVIF HDR 模式：色调映射为 SDR 后编码（8bit）");
    let sdr = to_sdr(captured, params);
    save_avif_sdr(&sdr, path)
}
