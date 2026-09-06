//! PNG 编码器
//!
//! - SDR：标准 8bit sRGB RGBA PNG
//! - HDR：16bit RGBA PNG + cICP chunk（BT.2020 原色 + PQ 传递函数，PNG v3 规范）

use std::io::Write;
use std::path::Path;

use crate::capture::hdr_pipeline::f16_to_f32;
use crate::capture::hdr_pipeline::HdrToSdrParams;
use crate::capture::hdr_pipeline::SdrImage;
use crate::capture::CapturedTexture;
use crate::color::{pq_oetf, PixelFormat};

/// 保存 SDR PNG（8bit sRGB + sRGB/gAMA/cHRM 色彩元数据）
pub fn save_sdr_png(image: &SdrImage, path: &Path) -> anyhow::Result<()> {
    let png_data = encode_sdr_png_bytes_with(image, png::Compression::Default)?;
    std::fs::write(path, &png_data)?;
    log::info!(
        "SDR PNG 已保存: {} ({}x{})",
        path.display(),
        image.width,
        image.height
    );
    Ok(())
}

/// 将 SDR 图像编码为 PNG 字节数据（内存中，不写文件）
///
/// 实时截图→标注路径使用：Fast 压缩优先低延迟（1080p 编码耗时约为
/// 默认压缩的 1/3），临时文件体积稍大可接受。
pub fn encode_sdr_png_bytes(image: &SdrImage) -> anyhow::Result<Vec<u8>> {
    let out = encode_sdr_png_bytes_with(image, png::Compression::Fast)?;
    log::info!("SDR PNG 已编码到内存: {}x{}", image.width, image.height);
    Ok(out)
}

/// 编码 SDR PNG（指定压缩级别）并在 IHDR 后插入 sRGB/gAMA/cHRM chunk
///
/// 元数据三件套（v2 文档 §3.7 / 附录 C）：sRGB 覆盖 gAMA/cHRM，
/// 后二者为旧解码器兜底；全部位于 IDAT 之前（PNG 规范要求）。
fn encode_sdr_png_bytes_with(
    image: &SdrImage,
    compression: png::Compression,
) -> anyhow::Result<Vec<u8>> {
    let mut png_buf: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_buf, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(compression);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&image.rgba)?;
        writer.finish()?;
    }
    Ok(insert_srgb_chunks(&png_buf))
}

/// 保存 HDR PNG（16bit + cICP BT.2020/PQ）
///
/// 将 scRGB/HDR10 线性光转为 PQ 编码的 16bit RGBA，并嵌入 cICP chunk。
pub fn save_hdr_png(
    captured: &CapturedTexture,
    path: &Path,
    params: &HdrToSdrParams,
) -> anyhow::Result<()> {
    // 1. 转换为 16bit PQ RGBA（大端序，PNG 规范要求）
    let data16 = to_pq16_rgba(captured, params);

    // 2. 用 png crate 编码 16bit RGBA 到内存
    let mut png_buf: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_buf, captured.width, captured.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Sixteen);
        encoder.set_compression(png::Compression::Default);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&data16)?;
        writer.finish()?;
    }

    // 3. 在 IHDR 之后插入 cICP chunk
    let final_png = insert_cicp_chunk(&png_buf);

    // 4. 写入文件
    let mut file = std::fs::File::create(path)?;
    file.write_all(&final_png)?;

    log::info!(
        "HDR PNG 已保存: {} ({}x{}, BT.2020/PQ)",
        path.display(),
        captured.width,
        captured.height
    );
    Ok(())
}

/// 将捕获的 HDR 纹理转为 16bit PQ RGBA（大端序）
///
/// scRGB → 绝对 nits 用物理常量 80（scRGB 1.0 = 80 nits，与显示器
/// SDR 白设置无关）；PQ 编码 0..1 = 0..10000 nits 绝对亮度。
/// params.input_sdr_white_nits 不参与本换算（仅 sidecar 记录/SDR 管线用）。
fn to_pq16_rgba(captured: &CapturedTexture, params: &HdrToSdrParams) -> Vec<u8> {
    let _ = params;
    const SCRGB_NITS_PER_UNIT: f32 = 80.0;
    let width = captured.width as usize;
    let height = captured.height as usize;
    let row_pitch = captured.row_pitch;
    let npx = width * height;
    let mut out = vec![0u8; npx * 8]; // RGBA16 = 8 bytes/pixel

    log::info!(
        "HDR PNG 编码: {}x{} format={:?} row_pitch={} scrgb_unit=80nits data_len={}",
        width,
        height,
        captured.format,
        row_pitch,
        captured.data.len()
    );

    match captured.format {
        PixelFormat::R16g16b16a16Float => {
            // scRGB: f16 线性光（BT.709 原色），1.0 = 80 nits（物理定义）
            // cICP 声明 BT.2020 原色 → 先做 BT.709→BT.2020 色域转换，
            // 否则查看器按 BT.2020 解读 BT.709 数据会偏红过饱和
            // （BT.2020→BT.709 显示转换中 R 通道增益 1.66 倍最大）
            let gamut = crate::color::bt709_to_bt2020();
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 8; // 4 × f16 = 8 bytes
                    let i = y * width + x;
                    let r = f16_to_f32(captured.data[s..s + 2].try_into().unwrap_or([0; 2]));
                    let g = f16_to_f32(captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
                    let b = f16_to_f32(captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
                    let (r, g, b) = gamut.apply(r, g, b);

                    let rn = (r * SCRGB_NITS_PER_UNIT / 10000.0).clamp(0.0, 1.0);
                    let gn = (g * SCRGB_NITS_PER_UNIT / 10000.0).clamp(0.0, 1.0);
                    let bn = (b * SCRGB_NITS_PER_UNIT / 10000.0).clamp(0.0, 1.0);

                    let rp = pq_oetf(rn);
                    let gp = pq_oetf(gn);
                    let bp = pq_oetf(bn);

                    write_u16_be(&mut out[i * 8..i * 8 + 2], rp);
                    write_u16_be(&mut out[i * 8 + 2..i * 8 + 4], gp);
                    write_u16_be(&mut out[i * 8 + 4..i * 8 + 6], bp);
                    write_u16_be(&mut out[i * 8 + 6..i * 8 + 8], 1.0); // A = 不透明
                }
            }

            // 诊断：采样中心像素
            let cx = width / 2;
            let cy = height / 2;
            let s = cy * row_pitch + cx * 8;
            let r = f16_to_f32(captured.data[s..s + 2].try_into().unwrap_or([0; 2]));
            let g = f16_to_f32(captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
            let b = f16_to_f32(captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
            let rn = (r * SCRGB_NITS_PER_UNIT / 10000.0).clamp(0.0, 1.0);
            let rp = pq_oetf(rn);
            let i = cy * width + cx;
            let out_r = u16::from_be_bytes([out[i * 8], out[i * 8 + 1]]);
            log::info!(
                "HDR PNG 诊断: 像素({},{}) scRGB=({:.3},{:.3},{:.3}) rn={:.4} pq={:.4} out_u16={}",
                cx,
                cy,
                r,
                g,
                b,
                rn,
                rp,
                out_r
            );
        }
        PixelFormat::R10g10b10a2 => {
            // HDR10: 已是 PQ 编码，只需扩展到 16bit
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 4;
                    let i = y * width + x;
                    let packed = u32::from_le_bytes([
                        captured.data[s],
                        captured.data[s + 1],
                        captured.data[s + 2],
                        captured.data[s + 3],
                    ]);
                    let r10 = (packed & 0x3FF) as f32 / 1023.0;
                    let g10 = ((packed >> 10) & 0x3FF) as f32 / 1023.0;
                    let b10 = ((packed >> 20) & 0x3FF) as f32 / 1023.0;

                    write_u16_be(&mut out[i * 8..i * 8 + 2], r10);
                    write_u16_be(&mut out[i * 8 + 2..i * 8 + 4], g10);
                    write_u16_be(&mut out[i * 8 + 4..i * 8 + 6], b10);
                    write_u16_be(&mut out[i * 8 + 6..i * 8 + 8], 1.0);
                }
            }
        }
        PixelFormat::Bgra8 => {
            // SDR 回退：sRGB（BT.709 原色）→ BT.2020 色域转换后装容器
            let gamut = crate::color::bt709_to_bt2020();
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 4;
                    let i = y * width + x;
                    let (r, g, b) = gamut.apply(
                        captured.data[s + 2] as f32 / 255.0,
                        captured.data[s + 1] as f32 / 255.0,
                        captured.data[s] as f32 / 255.0,
                    );
                    write_u16_be(&mut out[i * 8..i * 8 + 2], r);
                    write_u16_be(&mut out[i * 8 + 2..i * 8 + 4], g);
                    write_u16_be(&mut out[i * 8 + 4..i * 8 + 6], b);
                    write_u16_be(&mut out[i * 8 + 6..i * 8 + 8], 1.0);
                }
            }
        }
    }
    out
}

#[inline]
fn write_u16_be(buf: &mut [u8], val_f: f32) {
    let v = (val_f.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
    buf[0] = (v >> 8) as u8;
    buf[1] = (v & 0xFF) as u8;
}

/// PNG signature
const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// 在 IHDR 之后插入 sRGB + gAMA + cHRM chunk（SDR 输出的色彩元数据三件套）
///
/// - sRGB：`[0]`（rendering intent = Perceptual），声明文件为 sRGB 色彩空间
/// - gAMA：45455（= 1/2.2 × 100000），旧解码器兜底
/// - cHRM：BT.709 原色（白/红/绿/蓝 × 100000），旧解码器兜底
///
/// 顺序 sRGB → gAMA → cHRM → IDAT；sRGB 语义覆盖 gAMA/cHRM（规范允许并存）。
fn insert_srgb_chunks(png: &[u8]) -> Vec<u8> {
    // sig(8) + IHDR length(4) + type(4) + data(13) + crc(4) = 33
    if png.len() < PNG_SIG.len() + 25 {
        return png.to_vec();
    }
    let ihdr_end = PNG_SIG.len() + 25;

    // sRGB chunk：length=1, type="sRGB", data=[0]
    let srgb_type = *b"sRGB";
    let srgb_data = [0u8];
    let srgb_crc = crc32(&[&srgb_type[..], &srgb_data[..]].concat());

    // gAMA chunk：length=4, type="gAMA", data=45455 (big-endian)
    let gama_type = *b"gAMA";
    let gama_data = 45455u32.to_be_bytes();
    let gama_crc = crc32(&[&gama_type[..], &gama_data[..]].concat());

    // cHRM chunk：length=32, type="cHRM", data=8×u32 BE（BT.709 原色 ×100000）
    let chrm_type = *b"cHRM";
    let chrm_data: Vec<u8> = [
        31270u32, 32900, // white point
        64000, 33000, // red
        30000, 60000, // green
        15000, 6000, // blue
    ]
    .iter()
    .flat_map(|v| v.to_be_bytes())
    .collect();
    let chrm_crc = crc32(&[&chrm_type[..], &chrm_data[..]].concat());

    let mut result = Vec::with_capacity(png.len() + 16 + 16 + 48);
    result.extend_from_slice(&png[..ihdr_end]); // sig + IHDR
                                                // sRGB
    result.extend_from_slice(&1u32.to_be_bytes());
    result.extend_from_slice(&srgb_type);
    result.extend_from_slice(&srgb_data);
    result.extend_from_slice(&srgb_crc.to_be_bytes());
    // gAMA
    result.extend_from_slice(&4u32.to_be_bytes());
    result.extend_from_slice(&gama_type);
    result.extend_from_slice(&gama_data);
    result.extend_from_slice(&gama_crc.to_be_bytes());
    // cHRM
    result.extend_from_slice(&32u32.to_be_bytes());
    result.extend_from_slice(&chrm_type);
    result.extend_from_slice(&chrm_data);
    result.extend_from_slice(&chrm_crc.to_be_bytes());
    // 剩余部分（IDAT 起全部数据）
    result.extend_from_slice(&png[ihdr_end..]);
    result
}

/// 在 IHDR chunk 之后插入 cICP chunk
///
/// cICP 数据：[primaries, transfer, matrix, full_range]
/// - primaries = 9 (BT.2020)
/// - transfer = 16 (SMPTE ST 2084 PQ)
/// - matrix = 0 (RGB / identity)
/// - full_range = 1
fn insert_cicp_chunk(png: &[u8]) -> Vec<u8> {
    if png.len() < PNG_SIG.len() + 25 {
        return png.to_vec();
    }

    // PNG sig (8) + IHDR length(4) + type(4) + data(13) + crc(4) = 8 + 25 = 33
    let ihdr_end = PNG_SIG.len() + 25;

    // cICP chunk
    let cicp_type = *b"cICP";
    let cicp_data = [9u8, 16, 0, 1]; // BT.2020 / PQ / RGB / full range
    let crc = crc32(&[&cicp_type[..], &cicp_data].concat());

    let mut result = Vec::with_capacity(png.len() + 16);
    result.extend_from_slice(&png[..ihdr_end]); // sig + IHDR
                                                // cICP chunk
    result.extend_from_slice(&4u32.to_be_bytes()); // length
    result.extend_from_slice(&cicp_type);
    result.extend_from_slice(&cicp_data);
    result.extend_from_slice(&crc.to_be_bytes());
    // 剩余部分
    result.extend_from_slice(&png[ihdr_end..]);
    result
}

/// CRC32（IEEE 802.3，PNG 使用）
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析 PNG chunk 序列，返回 (type, data) 列表（校验 CRC）
    fn parse_chunks(png: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut off = 8; // 跳过 signature
        while off + 8 <= png.len() {
            let len = u32::from_be_bytes(png[off..off + 4].try_into().unwrap()) as usize;
            let ctype = String::from_utf8_lossy(&png[off + 4..off + 8]).into_owned();
            let data = &png[off + 8..off + 8 + len];
            let crc = u32::from_be_bytes(png[off + 8 + len..off + 12 + len].try_into().unwrap());
            assert_eq!(
                crc,
                crc32(&png[off + 4..off + 8 + len]),
                "chunk {} CRC 错误",
                ctype
            );
            let is_end = ctype == "IEND";
            out.push((ctype, data.to_vec()));
            off += 12 + len;
            if is_end {
                break;
            }
        }
        out
    }

    #[test]
    fn srgb_chunks_present_ordered_and_valid() {
        // 最小合法 PNG（1×1 RGBA，png crate 编码）
        let image = crate::capture::hdr_pipeline::SdrImage {
            width: 1,
            height: 1,
            rgba: vec![255, 128, 0, 255],
        };
        let png_data = encode_sdr_png_bytes_with(&image, png::Compression::Fast).unwrap();
        let chunks = parse_chunks(&png_data);
        let types: Vec<&str> = chunks.iter().map(|(t, _)| t.as_str()).collect();

        // 顺序：IHDR → sRGB → gAMA → cHRM → IDAT → IEND（元数据全部在 IDAT 前）
        let pos = |name: &str| types.iter().position(|t| *t == name);
        let (i_srgb, i_gama, i_chrm, i_idat) = (
            pos("sRGB").expect("缺 sRGB"),
            pos("gAMA").expect("缺 gAMA"),
            pos("cHRM").expect("缺 cHRM"),
            pos("IDAT").expect("缺 IDAT"),
        );
        assert!(
            i_srgb < i_gama && i_gama < i_chrm && i_chrm < i_idat,
            "chunk 顺序: {:?}",
            types
        );

        // 内容：sRGB intent 0；gAMA 45455；cHRM BT.709 原色
        assert_eq!(chunks[i_srgb].1, vec![0]);
        assert_eq!(
            u32::from_be_bytes(chunks[i_gama].1[..].try_into().unwrap()),
            45455
        );
        assert_eq!(chunks[i_chrm].1.len(), 32);
        assert_eq!(
            u32::from_be_bytes(chunks[i_chrm].1[0..4].try_into().unwrap()),
            31270
        );

        // png crate 重新解码校验整体合法性
        let decoder = png::Decoder::new(std::io::Cursor::new(&png_data));
        let mut reader = decoder.read_info().expect("解码失败");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        reader.next_frame(&mut buf).expect("解码帧失败");
    }
}
