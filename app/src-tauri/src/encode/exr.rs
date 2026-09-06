//! OpenEXR 编码器
//!
//! 将 HDR 线性光保存为 32bit 浮点 EXR。

use std::path::Path;

use exr::image::pixel_vec::PixelVec;
use exr::prelude::*;

use crate::capture::hdr_pipeline::f16_to_f32;
use crate::capture::CapturedTexture;
use crate::color::{pq_eotf, PixelFormat};

/// 保存 32bit 浮点 RGBA EXR
pub fn save_exr(captured: &CapturedTexture, path: &Path) -> anyhow::Result<()> {
    let width = captured.width as usize;
    let height = captured.height as usize;
    let pixels = extract_rgba_f32(captured);

    let resolution = Vec2(width, height);
    let pixel_vec = PixelVec::new(resolution, pixels);

    // SpecificChannels::rgba 接受任何 GetPixel<Pixel=(R,G,B,A)> 的存储
    let channels = SpecificChannels::rgba(pixel_vec);

    let layer = Layer::new(
        resolution,
        LayerAttributes::named("jietu-hdr screenshot"),
        Encoding::default(),
        channels,
    );

    // 单层图像：直接 from_layer 自动构建 ImageAttributes（位置 0,0 大小=resolution）
    let image = Image::from_layer(layer);

    image
        .write()
        .to_file(path)
        .map_err(|e| anyhow::anyhow!("EXR 编码失败: {}", e))?;

    log::info!(
        "EXR 已保存: {} ({}x{}, 32bit float)",
        path.display(),
        captured.width,
        captured.height
    );
    Ok(())
}

/// 从捕获纹理提取 RGBA f32 线性光（元组形式，匹配 SpecificChannels::rgba）
fn extract_rgba_f32(captured: &CapturedTexture) -> Vec<(f32, f32, f32, f32)> {
    let width = captured.width as usize;
    let height = captured.height as usize;
    let row_pitch = captured.row_pitch;
    let npx = width * height;
    let mut out = Vec::with_capacity(npx);

    match captured.format {
        PixelFormat::R16g16b16a16Float => {
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 8;
                    let r = f16_to_f32(captured.data[s..s + 2].try_into().unwrap_or([0; 2]));
                    let g = f16_to_f32(captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
                    let b = f16_to_f32(captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
                    out.push((r, g, b, 1.0));
                }
            }
        }
        PixelFormat::R10g10b10a2 => {
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 4;
                    let packed = u32::from_le_bytes([
                        captured.data[s],
                        captured.data[s + 1],
                        captured.data[s + 2],
                        captured.data[s + 3],
                    ]);
                    let r10 = (packed & 0x3FF) as f32 / 1023.0;
                    let g10 = ((packed >> 10) & 0x3FF) as f32 / 1023.0;
                    let b10 = ((packed >> 20) & 0x3FF) as f32 / 1023.0;
                    out.push((pq_eotf(r10), pq_eotf(g10), pq_eotf(b10), 1.0));
                }
            }
        }
        PixelFormat::Bgra8 => {
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 4;
                    let r = crate::color::srgb_eotf(captured.data[s + 2] as f32 / 255.0);
                    let g = crate::color::srgb_eotf(captured.data[s + 1] as f32 / 255.0);
                    let b = crate::color::srgb_eotf(captured.data[s] as f32 / 255.0);
                    out.push((r, g, b, 1.0));
                }
            }
        }
    }
    out
}
