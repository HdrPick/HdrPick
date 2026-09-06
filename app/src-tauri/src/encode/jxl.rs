//! JPEG XL 编码器（基于 libjxl C 库，通过 jxl-sys 绑定）
//!
//! HDR：16bit RGB，BT.2020 原色 + PQ 传递函数（HDR10）

use std::ffi::c_void;
use std::path::Path;
use std::ptr;

use jxl_sys as jxl;

use crate::capture::hdr_pipeline::{f16_to_f32, HdrToSdrParams};
use crate::capture::CapturedTexture;
use crate::color::{pq_oetf, PixelFormat};
use crate::encode::QualityLevel;

/// 保存 HDR JPEG XL（BT.2020/PQ RGB，12/16bit 可选）
///
/// quality：5 档编码质量（Lossless = 数学无损 modular；有损档 VarDCT）
/// depth：位深（12 / 16；libjxl 支持任意 1-16 整数位深，12bit 体积更小）
pub fn save_jxl(
    captured: &CapturedTexture,
    path: &Path,
    params: &HdrToSdrParams,
    quality: QualityLevel,
    depth: u32,
) -> anyhow::Result<()> {
    let width = captured.width;
    let height = captured.height;
    let depth = depth.clamp(8, 16);

    // 转换为 PQ RGB（3 通道，little-endian uint16 容器，按 depth 量化）
    let pixels = to_pq_rgb(captured, params, depth);

    let output = encode_jxl(width, height, &pixels, 3, quality, depth)?; // RGB

    std::fs::write(path, &output)?;
    log::info!(
        "HDR JXL 已保存: {} ({}x{}, BT.2020/PQ, {}bit)",
        path.display(),
        width,
        height,
        depth
    );
    Ok(())
}

/// SDR 图保存为 HDR JXL（AI 放大增值输出）
///
/// 输入：交织 RGBA f32（sRGB 编码值域 [0,1]，alpha 线性不透明度，None=不透明）
/// 管线：sdr_to_hdr_nits（EOTF + 高光扩展 + 饱和度，见 color::sdr_hdr）
///       → BT.709→BT.2020 原色 → PQ OETF → 16bit
///
/// SDR 模拟还原到 HDR：sRGB 1.0 = "SDR 显示时的白"，映射到系统 SDR 参考白
/// （DisplayConfig SDR_WHITE_LEVEL，典型 200nits）——HDR 屏正是按该值渲染 SDR 内容，
/// 如此 HDR 显示亮度才与 SDR 模式等效。此前硬编码 80nits：参考白 200 的机器上
/// 显示只有系统 SDR 亮度的 40%（偏暗），即"SDR→HDR 未还原"。
/// 高光扩展（highlight_scale>1）在参考白之上叠真 HDR 高光（中间调保持等效）。
/// （对比：截图路径 to_pq_rgb 用物理常量 80——DDA 捕获的 scRGB 已是系统渲染后的
/// 绝对物理光，SDR 内容在数据里本来就是 2.5@200nits，两者不矛盾。）
pub fn save_sdr_as_jxl_hdr(
    width: u32,
    height: u32,
    rgba: &[f32], // RGBA 交织，len = w*h*4；alpha 全 1 时传 None 更省
    has_alpha: bool,
    path: &Path,
    quality: QualityLevel,
    depth: u32,
    enhance: crate::color::SdrToHdrParams,
    ai_nits: Option<Vec<f32>>, // AI 档预计算 nits（BT.709 线性 CHW）；Some 时跳过曲线管线
) -> anyhow::Result<()> {
    let depth = depth.clamp(8, 16);
    let max_val = ((1u32 << depth) - 1) as f32;
    let gamut = crate::color::bt709_to_bt2020();
    let npx = (width as usize) * (height as usize);
    // 全图增强管线：自适应拐点 + 色域外推 + 高光扩展 + 过曝缓解
    // （sRGB 编码域 CHW → HDR 线性 nits CHW，BT.709 原色域）
    // AI 档：ITM 网络已直出 nits，跳过曲线
    let nits_img = match ai_nits {
        Some(n) => n,
        None => crate::color::sdr_to_hdr_image(rgba, width as usize, height as usize, &enhance),
    };
    // 管线：HDR nits → BT.2020 色域矩阵 → PQ OETF → uint16 容器按 depth 量化
    let rgb_to_pq16 = |r: f32, g: f32, b: f32| -> [u16; 3] {
        let (r2, g2, b2) = gamut.apply(r, g, b);
        let enc = |lin709_nits: f32| -> u16 {
            // BT.709→BT.2020 越界（负值/超峰值）夹紧到有效 PQ 域
            let pq = crate::color::pq_oetf(lin709_nits.clamp(0.0, 10000.0) / 10000.0);
            (pq.clamp(0.0, 1.0) * max_val).round() as u16
        };
        [enc(r2), enc(g2), enc(b2)]
    };

    let channels = if has_alpha { 4 } else { 3 };
    let mut pixels = vec![0u8; npx * channels * 2];
    for i in 0..npx {
        let a = rgba[i * 4 + 3];
        let [pr, pg, pb] = rgb_to_pq16(nits_img[i], nits_img[npx + i], nits_img[2 * npx + i]);
        let base = i * channels * 2;
        pixels[base..base + 2].copy_from_slice(&pr.to_le_bytes());
        pixels[base + 2..base + 4].copy_from_slice(&pg.to_le_bytes());
        pixels[base + 4..base + 6].copy_from_slice(&pb.to_le_bytes());
        if has_alpha {
            let pa = (a.clamp(0.0, 1.0) * max_val).round() as u16;
            pixels[base + 6..base + 8].copy_from_slice(&pa.to_le_bytes());
        }
    }

    let output = encode_jxl(width, height, &pixels, channels as u32, quality, depth)?;
    std::fs::write(path, &output)?;
    log::info!(
        "SDR→HDR JXL 已保存: {} ({}x{}, BT.2020/PQ, {}bit, alpha={}, 白={:.0}nits 高光={:.1}× 外推={:.2})",
        path.display(),
        width,
        height,
        depth,
        has_alpha,
        enhance.diffuse_white_nits,
        enhance.highlight_scale,
        enhance.gamut_extrapolate
    );
    Ok(())
}

fn encode_jxl(
    width: u32,
    height: u32,
    pixels: &[u8],
    num_channels: u32,
    quality: QualityLevel,
    depth: u32,
) -> anyhow::Result<Vec<u8>> {
    unsafe {
        // 1. 创建 encoder
        let enc = jxl::JxlEncoderCreate(ptr::null());
        if enc.is_null() {
            anyhow::bail!("JxlEncoderCreate 返回 null");
        }
        let _enc_guard = EncoderGuard(enc);

        // 2. 创建并设置并行 runner
        let num_threads = num_cpus::get().max(1);
        let runner = jxl::JxlThreadParallelRunnerCreate(ptr::null(), num_threads);
        if runner.is_null() {
            anyhow::bail!("JxlThreadParallelRunnerCreate 返回 null");
        }
        let _runner_guard = RunnerGuard(runner);

        let status =
            jxl::JxlEncoderSetParallelRunner(enc, Some(jxl::JxlThreadParallelRunner), runner);
        check_status(status, "JxlEncoderSetParallelRunner")?;

        // 3. 设置 BasicInfo（uint HDR10，depth 位深：12 体积优先 / 16 精度优先）
        let mut basic_info = jxl::JxlBasicInfo::default();
        basic_info.xsize = width;
        basic_info.ysize = height;
        basic_info.bits_per_sample = depth;
        basic_info.exponent_bits_per_sample = 0; // UINT
        basic_info.orientation = jxl::JxlOrientation::Identity;
        basic_info.num_color_channels = 3;
        basic_info.num_extra_channels = if num_channels == 4 { 1 } else { 0 };
        basic_info.alpha_bits = if num_channels == 4 { depth } else { 0 };
        basic_info.alpha_exponent_bits = 0;
        // 无损档必须（modular 逐像素可还原 + 禁 XYB 色彩变换保留原色）；
        // 有损档置 0 允许 libjxl 走 XYB/VarDCT 路径换取压缩率
        basic_info.uses_original_profile = if quality.is_lossless() { 1 } else { 0 };
        basic_info.intensity_target = 10000.0; // PQ 峰值
        basic_info.min_nits = 0.0;

        let status = jxl::JxlEncoderSetBasicInfo(enc, &basic_info);
        check_status(status, "JxlEncoderSetBasicInfo")?;

        // 4. 设置 ColorEncoding（BT.2020 原色 + PQ）
        let mut color = jxl::JxlColorEncoding::default();
        color.color_space = jxl::JxlColorSpace::Rgb;
        color.white_point = jxl::JxlWhitePoint::D65;
        color.rendering_intent = jxl::JxlRenderingIntent::Perceptual;
        color.primaries = jxl::JxlPrimaries::P2100; // BT.2020
        color.transfer_function = jxl::JxlTransferFunction::Pq;

        let status = jxl::JxlEncoderSetColorEncoding(enc, &color);
        check_status(status, "JxlEncoderSetColorEncoding")?;

        // 5. 创建 FrameSettings 并设置帧头
        let frame_settings = jxl::JxlEncoderFrameSettingsCreate(enc, ptr::null());
        if frame_settings.is_null() {
            anyhow::bail!("JxlEncoderFrameSettingsCreate 返回 null");
        }

        // 距离档位：0.0=无损（modular）→ 1.0=体积优先（VarDCT）。
        // 2K 参考体积：无损 2-8MB / 极高 ~1-3MB / 平衡 <1MB
        let distance = quality.jxl_distance();
        let status = jxl::JxlEncoderSetFrameDistance(frame_settings, distance);
        check_status(status, "JxlEncoderSetFrameDistance")?;

        // 输入像素按 BasicInfo.bits_per_sample 量程解读（FROM_CODESTREAM）。
        // 默认 FROM_PIXEL_FORMAT 对 Uint16 固定按 65535 归一——12bit 数据
        // （已按 max_val=4095 量化）会被整体缩小 16 倍 → PQ 底部 → 纯黑。
        // 16bit 时两者量程恰好相同，故此前 16bit 输出正常而 12bit 全黑。
        let bit_depth = jxl::JxlBitDepth {
            type_: jxl::JxlBitDepthType::FromCodestream,
            ..Default::default()
        };
        let status = jxl::JxlEncoderSetFrameBitDepth(frame_settings, &bit_depth);
        check_status(status, "JxlEncoderSetFrameBitDepth")?;

        let mut frame_header = jxl::JxlFrameHeader::default();
        frame_header.is_last = 1; // JXL_TRUE
        let status = jxl::JxlEncoderSetFrameHeader(frame_settings, &frame_header);
        check_status(status, "JxlEncoderSetFrameHeader")?;

        // 6. 添加图像帧（16bit uint16）
        let pixel_format = jxl::JxlPixelFormat {
            num_channels,
            data_type: jxl::JxlDataType::Uint16,
            endianness: jxl::JxlEndianness::Little,
            align: 0,
        };

        let status = jxl::JxlEncoderAddImageFrame(
            frame_settings,
            &pixel_format,
            pixels.as_ptr() as *const c_void,
            pixels.len(),
        );
        check_status(status, "JxlEncoderAddImageFrame")?;

        // 7. 关闭输入
        jxl::JxlEncoderCloseInput(enc);

        // 8. 处理输出
        let mut output: Vec<u8> = Vec::with_capacity(1 << 20);
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let mut next_out = buf.as_mut_ptr();
            let mut avail_out = buf.len();
            let status = jxl::JxlEncoderProcessOutput(enc, &mut next_out, &mut avail_out);
            let written = buf.len() - avail_out;
            output.extend_from_slice(&buf[..written]);

            match status {
                jxl::JxlEncoderStatus::Success => break,
                jxl::JxlEncoderStatus::NeedMoreOutput => continue,
                _ => anyhow::bail!("JxlEncoderProcessOutput 失败: {:?}", status),
            }
        }

        Ok(output)
    }
}

fn check_status(status: jxl::JxlEncoderStatus, ctx: &str) -> anyhow::Result<()> {
    match status {
        jxl::JxlEncoderStatus::Success => Ok(()),
        _ => anyhow::bail!("{} 失败: {:?}", ctx, status),
    }
}

struct EncoderGuard(*mut jxl::JxlEncoder);
impl Drop for EncoderGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { jxl::JxlEncoderDestroy(self.0) };
        }
    }
}

struct RunnerGuard(*mut c_void);
impl Drop for RunnerGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { jxl::JxlThreadParallelRunnerDestroy(self.0) };
        }
    }
}

/// 将 HDR 纹理转为 PQ RGB（3 通道，little-endian uint16 容器，按 depth 量化）
///
/// scRGB → 绝对 nits 用物理常量 80（scRGB 1.0 = 80 nits，与显示器
/// SDR 白设置无关）；PQ 编码 0..1 = 0..10000 nits 绝对亮度。
/// params.input_sdr_white_nits 不参与本换算（仅 sidecar 记录/SDR 管线用）。
fn to_pq_rgb(captured: &CapturedTexture, params: &HdrToSdrParams, depth: u32) -> Vec<u8> {
    let max_val = ((1u32 << depth.clamp(8, 16)) - 1) as f32;
    let _ = params;
    const SCRGB_NITS_PER_UNIT: f32 = 80.0;
    let width = captured.width as usize;
    let height = captured.height as usize;
    let row_pitch = captured.row_pitch;
    let npx = width * height;
    let mut out = vec![0u8; npx * 6]; // RGB16 = 6 bytes/pixel

    // 诊断：采样中心像素的原始值，确认 HDR 数据范围
    if npx > 0 && matches!(captured.format, PixelFormat::R16g16b16a16Float) {
        let cx = width / 2;
        let cy = height / 2;
        let s = cy * row_pitch + cx * 8;
        if s + 6 <= captured.data.len() {
            let r = f16_to_f32(captured.data[s..s + 2].try_into().unwrap_or([0; 2]));
            let g = f16_to_f32(captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
            let b = f16_to_f32(captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
            log::info!(
                "JXL 诊断: scrgb_unit=80nits, 中心像素 scRGB=({:.3},{:.3},{:.3}) → nits=({:.0},{:.0},{:.0})",
                r, g, b,
                r * SCRGB_NITS_PER_UNIT, g * SCRGB_NITS_PER_UNIT, b * SCRGB_NITS_PER_UNIT
            );
        }
    }

    match captured.format {
        PixelFormat::R16g16b16a16Float => {
            // scRGB：f16 线性光（BT.709 原色），1.0 = 80 nits（物理定义）
            // ColorEncoding 声明 BT.2020 原色 → 先做 BT.709→BT.2020 色域转换，
            // 否则查看器按 BT.2020 解读 BT.709 数据会偏红过饱和
            // （BT.2020→BT.709 显示转换中 R 通道增益 1.66 倍最大）
            let gamut = crate::color::bt709_to_bt2020();
            for y in 0..height {
                let src_off = y * row_pitch;
                for x in 0..width {
                    let s = src_off + x * 8;
                    let i = y * width + x;
                    let r = f16_to_f32(captured.data[s..s + 2].try_into().unwrap_or([0; 2]));
                    let g = f16_to_f32(captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
                    let b = f16_to_f32(captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
                    let (r, g, b) = gamut.apply(r, g, b);

                    let rn = (r * SCRGB_NITS_PER_UNIT / 10000.0).clamp(0.0, 1.0);
                    let gn = (g * SCRGB_NITS_PER_UNIT / 10000.0).clamp(0.0, 1.0);
                    let bn = (b * SCRGB_NITS_PER_UNIT / 10000.0).clamp(0.0, 1.0);

                    write_u16_le(&mut out[i * 6..i * 6 + 2], pq_oetf(rn), max_val);
                    write_u16_le(&mut out[i * 6 + 2..i * 6 + 4], pq_oetf(gn), max_val);
                    write_u16_le(&mut out[i * 6 + 4..i * 6 + 6], pq_oetf(bn), max_val);
                }
            }
        }
        PixelFormat::R10g10b10a2 => {
            // HDR10：已是 PQ 编码，只需扩展到 16bit
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

                    write_u16_le(&mut out[i * 6..i * 6 + 2], r10, max_val);
                    write_u16_le(&mut out[i * 6 + 2..i * 6 + 4], g10, max_val);
                    write_u16_le(&mut out[i * 6 + 4..i * 6 + 6], b10, max_val);
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
                    write_u16_le(&mut out[i * 6..i * 6 + 2], r, max_val);
                    write_u16_le(&mut out[i * 6 + 2..i * 6 + 4], g, max_val);
                    write_u16_le(&mut out[i * 6 + 4..i * 6 + 6], b, max_val);
                }
            }
        }
    }
    out
}

#[inline]
fn write_u16_le(buf: &mut [u8], val_f: f32, max_val: f32) {
    let v = (val_f.clamp(0.0, 1.0) * max_val + 0.5) as u16;
    buf[0] = (v & 0xFF) as u8;
    buf[1] = (v >> 8) as u8;
}

// ============================================================================
// 动画 JXL 编码器（录屏 record 模块用；流式：逐帧 AddImageFrame + 增量写盘）
// ============================================================================

/// 录屏质量档 → libjxl effort（编码速度：越低越快质量越低）
///
/// 满变化率 30s 场景的编码耗时主要由 effort 决定（见 docs/录屏JXL动图设计方案.md §4.4.3）
pub fn animation_effort(quality: QualityLevel) -> i64 {
    match quality {
        QualityLevel::Lossless => 3, // modular 快档
        QualityLevel::VeryHigh => 4,
        QualityLevel::High => 3,
        QualityLevel::Balanced => 3,
        QualityLevel::Compact => 4,
    }
}

/// 动画 JXL 编码器（BT.2020/PQ 16bit RGB，逐帧时长，无限循环）
///
/// 输入像素格式：PQ16 RGB 紧密排布（3 通道 × uint16 little-endian = 6 字节/像素），
/// 与单帧 `save_jxl` 的 `to_pq16_rgb` 输出一致（record 走 GPU 同数学管线产出）。
///
/// 生命周期：`new` → 每帧 `add_frame`（duration 单位 ms；仅末帧 is_last=true）
/// → `finalize`（CloseInput + flush 落盘）。
pub struct AnimationJxlEncoder {
    enc: *mut jxl::JxlEncoder,
    runner: *mut c_void,
    frame_settings: *mut jxl::JxlEncoderFrameSettings,
    out_file: std::fs::File,
    width: u32,
    height: u32,
    frames: u64,
}

impl Drop for AnimationJxlEncoder {
    fn drop(&mut self) {
        // frame_settings 由 encoder 内部持有，无需单独销毁
        if !self.enc.is_null() {
            unsafe { jxl::JxlEncoderDestroy(self.enc) };
        }
        if !self.runner.is_null() {
            unsafe { jxl::JxlThreadParallelRunnerDestroy(self.runner) };
        }
    }
}

impl AnimationJxlEncoder {
    /// 创建动画编码器（创建输出文件 + 配置 BasicInfo/ColorEncoding/质量档）
    ///
    /// `effort`：libjxl 编码速度档 1..7（录制用 `animation_effort()` 推荐值）
    pub fn new(
        path: &Path,
        width: u32,
        height: u32,
        quality: QualityLevel,
        effort: i64,
    ) -> anyhow::Result<Self> {
        let out_file = std::fs::File::create(path)?;
        unsafe {
            let enc = jxl::JxlEncoderCreate(ptr::null());
            if enc.is_null() {
                anyhow::bail!("JxlEncoderCreate 返回 null");
            }
            let _enc_guard = EncoderGuard(enc);

            // 并行 runner：限核（≤核数-2，clamp 1..8）避免与前台应用抢 CPU
            let threads = num_cpus::get().saturating_sub(2).clamp(1, 8);
            let runner = jxl::JxlThreadParallelRunnerCreate(ptr::null(), threads);
            if runner.is_null() {
                anyhow::bail!("JxlThreadParallelRunnerCreate 返回 null");
            }
            let _runner_guard = RunnerGuard(runner);

            let status =
                jxl::JxlEncoderSetParallelRunner(enc, Some(jxl::JxlThreadParallelRunner), runner);
            check_status(status, "JxlEncoderSetParallelRunner")?;

            // BasicInfo：16bit uint PQ + 动画（1000 tps = 毫秒级 duration）
            let mut basic_info = jxl::JxlBasicInfo::default();
            basic_info.xsize = width;
            basic_info.ysize = height;
            basic_info.bits_per_sample = 16;
            basic_info.exponent_bits_per_sample = 0; // UINT
            basic_info.orientation = jxl::JxlOrientation::Identity;
            basic_info.num_color_channels = 3;
            basic_info.num_extra_channels = 0;
            basic_info.alpha_bits = 0;
            basic_info.alpha_exponent_bits = 0;
            // 无损档必须（modular + 禁 XYB 保留原色）；有损档走 VarDCT
            basic_info.uses_original_profile = if quality.is_lossless() { 1 } else { 0 };
            basic_info.intensity_target = 10000.0; // PQ 峰值
            basic_info.min_nits = 0.0;
            basic_info.have_animation = 1;
            basic_info.animation.tps_numerator = 1000;
            basic_info.animation.tps_denominator = 1;
            basic_info.animation.num_loops = 0; // 无限循环
            basic_info.animation.have_timecodes = 0;

            let status = jxl::JxlEncoderSetBasicInfo(enc, &basic_info);
            check_status(status, "JxlEncoderSetBasicInfo")?;

            // ColorEncoding：BT.2020 原色 + PQ（与单帧 HDR JXL 一致）
            let mut color = jxl::JxlColorEncoding::default();
            color.color_space = jxl::JxlColorSpace::Rgb;
            color.white_point = jxl::JxlWhitePoint::D65;
            color.rendering_intent = jxl::JxlRenderingIntent::Perceptual;
            color.primaries = jxl::JxlPrimaries::P2100; // BT.2020
            color.transfer_function = jxl::JxlTransferFunction::Pq;
            let status = jxl::JxlEncoderSetColorEncoding(enc, &color);
            check_status(status, "JxlEncoderSetColorEncoding")?;

            let frame_settings = jxl::JxlEncoderFrameSettingsCreate(enc, ptr::null());
            if frame_settings.is_null() {
                anyhow::bail!("JxlEncoderFrameSettingsCreate 返回 null");
            }

            // 质量档（帧设置全帧共用）：distance + effort
            let status = jxl::JxlEncoderSetFrameDistance(frame_settings, quality.jxl_distance());
            check_status(status, "JxlEncoderSetFrameDistance")?;
            let status = jxl::JxlEncoderFrameSettingsSetOption(
                frame_settings,
                jxl::JXL_ENC_FRAME_SETTING_EFFORT,
                effort,
            );
            check_status(status, "JxlEncoderFrameSettingsSetOption(EFFORT)")?;
            // 解码速度档 4（最大，录制专用）：0-4 档逐级用体积换解码速度——
            // 4MP@60fps 回放预算 16.7ms/帧，档 1 实测 ~75ms/帧（吞吐差 4 倍+），
            // 预缓冲耗尽后必然卡帧。档 4 禁用高成本解码特性，解码提速数倍，
            // 代价仅码流体积增大（屏幕录制场景体积不敏感）
            let status = jxl::JxlEncoderFrameSettingsSetOption(
                frame_settings,
                jxl::JXL_ENC_FRAME_SETTING_DECODING_SPEED,
                4,
            );
            check_status(status, "JxlEncoderFrameSettingsSetOption(DECODING_SPEED)")?;

            // 强制独立帧（每帧关键帧）——禁用两项跨帧引用：
            // ① patches（文本块引用：帧 N+1 引用帧 N 的图像块省码率）
            // ② progressive DC（DC 帧引用）
            // 这两项让解码端帧间依赖（实测 SkipFrames 后解下一帧崩溃），
            // 独立帧后多解码器可并行填充回放缓冲（吞吐 ×N），且随机访问/
            // 拖动进度成为可能。代价：文件体积 +10~20%（录制场景可接受）。
            let status = jxl::JxlEncoderFrameSettingsSetOption(
                frame_settings,
                jxl::JXL_ENC_FRAME_SETTING_PATCHES,
                -1, // Override::kOff
            );
            check_status(status, "JxlEncoderFrameSettingsSetOption(PATCHES=off)")?;
            let status = jxl::JxlEncoderFrameSettingsSetOption(
                frame_settings,
                jxl::JXL_ENC_FRAME_SETTING_PROGRESSIVE_DC,
                0,
            );
            check_status(status, "JxlEncoderFrameSettingsSetOption(PROGRESSIVE_DC=0)")?;

            // 所有权移交 Self（其 Drop 负责销毁）；错误路径由守卫清理
            std::mem::forget(_enc_guard);
            std::mem::forget(_runner_guard);
            Ok(AnimationJxlEncoder {
                enc,
                runner,
                frame_settings,
                out_file,
                width,
                height,
                frames: 0,
            })
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// 添加一帧（PQ16 RGB 紧密数据，6 字节/像素）
    ///
    /// `duration_ms`：本帧持续时长；`is_last`：仅末帧 true
    pub fn add_frame(
        &mut self,
        pq16_rgb: &[u8],
        duration_ms: u64,
        is_last: bool,
    ) -> anyhow::Result<()> {
        let expected = self.width as usize * self.height as usize * 6;
        if pq16_rgb.len() != expected {
            anyhow::bail!(
                "帧数据长度不符: {} 期望 {}（{}x{} PQ16 RGB）",
                pq16_rgb.len(),
                expected,
                self.width,
                self.height
            );
        }
        unsafe {
            let mut frame_header = jxl::JxlFrameHeader::default();
            frame_header.duration = duration_ms.min(u32::MAX as u64) as u32;
            frame_header.is_last = if is_last { 1 } else { 0 };
            let status = jxl::JxlEncoderSetFrameHeader(self.frame_settings, &frame_header);
            check_status(status, "JxlEncoderSetFrameHeader")?;

            let pixel_format = jxl::JxlPixelFormat {
                num_channels: 3,
                data_type: jxl::JxlDataType::Uint16,
                endianness: jxl::JxlEndianness::Little,
                align: 0,
            };
            let status = jxl::JxlEncoderAddImageFrame(
                self.frame_settings,
                &pixel_format,
                pq16_rgb.as_ptr() as *const c_void,
                pq16_rgb.len(),
            );
            check_status(status, "JxlEncoderAddImageFrame")?;
        }
        self.frames += 1;
        // 逐帧增量落盘（30s 录制产物几十 MB，写盘非关键路径）
        self.flush_output()
    }

    /// 收尾：CloseInput + flush 全部剩余输出
    pub fn finalize(mut self) -> anyhow::Result<()> {
        use std::io::Write;
        if self.frames == 0 {
            anyhow::bail!("动画无有效帧，无法 finalize");
        }
        unsafe { jxl::JxlEncoderCloseInput(self.enc) };
        self.flush_output()?;
        self.out_file.flush()?;
        Ok(())
    }

    /// ProcessOutput 循环 → 增量写文件（Success = 当前所有输出已取尽）
    fn flush_output(&mut self) -> anyhow::Result<()> {
        use std::io::Write;
        unsafe {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let mut next_out = buf.as_mut_ptr();
                let mut avail_out = buf.len();
                let status = jxl::JxlEncoderProcessOutput(self.enc, &mut next_out, &mut avail_out);
                let written = buf.len() - avail_out;
                self.out_file.write_all(&buf[..written])?;
                match status {
                    jxl::JxlEncoderStatus::Success => break,
                    jxl::JxlEncoderStatus::NeedMoreOutput => continue,
                    _ => anyhow::bail!("JxlEncoderProcessOutput 失败: {:?}", status),
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立帧（关键帧）验证：禁用 patches/progressive_dc 后，SkipFrames 随机
    /// 访问必须成功（旧产物此实验 STATUS_ACCESS_VIOLATION 崩溃——帧间引用）
    #[test]
    fn independent_frames_random_access() {
        let (w, h) = (256u32, 128u32);
        let dir = std::env::temp_dir().join("jietu_enc_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("indep_anim_test.jxl");
        let quality = QualityLevel::High;
        let mut enc = AnimationJxlEncoder::new(&path, w, h, quality, animation_effort(quality))
            .unwrap();
        for i in 0..20u16 {
            let mut buf = vec![0u8; (w as usize) * (h as usize) * 6];
            for p in buf.chunks_exact_mut(6) {
                let v = i as u16 * 3000;
                p[0..2].copy_from_slice(&v.to_le_bytes());
                p[2..4].copy_from_slice(&v.to_le_bytes());
                p[4..6].copy_from_slice(&(v / 2).to_le_bytes());
            }
            enc.add_frame(&buf, 40, i == 19).unwrap();
        }
        enc.finalize().unwrap();

        let data = std::fs::read(&path).unwrap();
        unsafe {
            let dec = jxl::JxlDecoderCreate(ptr::null());
            struct G(*mut jxl::JxlDecoder);
            impl Drop for G {
                fn drop(&mut self) {
                    unsafe { jxl::JxlDecoderDestroy(self.0) };
                }
            }
            let _g = G(dec);
            let events = jxl::JXL_DEC_BASIC_INFO | jxl::JXL_DEC_FRAME | jxl::JXL_DEC_FULL_IMAGE;
            assert_eq!(
                jxl::JxlDecoderSubscribeEvents(dec, events),
                jxl::JxlDecoderStatus::Success
            );
            assert_eq!(
                jxl::JxlDecoderSetInput(dec, data.as_ptr(), data.len()),
                jxl::JxlDecoderStatus::Success
            );
            jxl::JxlDecoderCloseInput(dec);
            jxl::JxlDecoderSkipFrames(dec, 7);
            let mut got_full = false;
            // 输出缓冲生命周期覆盖整个解码过程（解码器持有指针直到 FullImage）
            let mut bytes = vec![0u8; (w as usize) * (h as usize) * 3 * 2];
            loop {
                match jxl::JxlDecoderProcessInput(dec) {
                    jxl::JxlDecoderStatus::Frame => continue,
                    jxl::JxlDecoderStatus::NeedImageOutBuffer => {
                        let fmt = jxl::JxlPixelFormat {
                            num_channels: 3,
                            data_type: jxl::JxlDataType::Uint16,
                            endianness: jxl::JxlEndianness::Little,
                            align: 0,
                        };
                        jxl::JxlDecoderSetImageOutBuffer(
                            dec,
                            &fmt,
                            bytes.as_mut_ptr() as *mut std::ffi::c_void,
                            bytes.len(),
                        );
                        continue;
                    }
                    jxl::JxlDecoderStatus::FullImage => {
                        got_full = true;
                        break;
                    }
                    jxl::JxlDecoderStatus::Error => {
                        panic!("独立帧解码失败：SkipFrames(7) 后 Error（仍有跨帧引用？）");
                    }
                    // Success/NeedMoreInput：继续推进（输入未耗尽/未到目标帧）
                    _ => continue,
                }
            }
            assert!(got_full, "SkipFrames 后应解出帧");
            // 中心像素取值：编码时每帧 i 全屏纯色 v=i*3000，B=v/2。
            // 解出的应是某一帧的纯色——读 R 通道值反推帧号并验证纯色一致性
            let px = |x: usize, y: usize| -> u16 {
                let off = (y * w as usize + x) * 3;
                u16::from_le_bytes([bytes[off * 2], bytes[off * 2 + 1]])
            };
            let (c0, c1, c2) = (px(10, 10), px(w as usize / 2, h as usize / 2), px(200, 100));
            // SkipFrames(7) 语义：跳过接下来 7 帧 → 应解出第 8 帧（idx=7）
            let expect = 7u16 * 3000;
            assert!(
                (c0 as i32 - expect as i32).abs() < 1500
                    && (c1 as i32 - expect as i32).abs() < 1500
                    && (c2 as i32 - expect as i32).abs() < 1500,
                "跳帧后像素应为帧 7 的颜色（≈{expect}，实际 {}/{}/{}——帧间引用残留或 skip 语义偏差）",
                c0, c1, c2
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}
