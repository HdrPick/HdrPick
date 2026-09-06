//! HDR → SDR 转换管线（v2）
//!
//! 完整管线（docs/hdr-tonemap-presets-v2.md §4）：
//! ```text
//! 输入清洗(NaN/Inf) → 统一为绝对 nits（scRGB×80 / HDR10 PQ EOTF×10000 + BT.2020→709）
//! → 曝光(2^ev) → 亮度驱动 tone map（ToneCurve）→ RGB 按 Y_out/Y_in 等比缩放
//! → 饱和度 → 中性轴色域压缩（k_max 钳制入 [0,1]）→ 安全 clamp
//! → sRGB OETF → 编码域对比度 → 零均值 Bayer 抖动量化
//! ```
//!
//! 关键语义修正（相对旧管线）：**scRGB 1.0 = 80 nit（绝对）**，
//! 显示器 SDR 白（如 200 nit）在捕获数据中表现为 scRGB ≈ 2.5；
//! 归一化基准 `input_sdr_white_nits` 仅用于把 SDR 白对齐到 y_rel = 1.0。

use std::sync::atomic::{AtomicU32, Ordering};

use crate::capture::CapturedTexture;
use crate::color::transfer::{pq_eotf, srgb8_quantize, srgb8_quantize_dithered, srgb_oetf};
use crate::color::{bt2020_to_bt709, Mat3, PixelFormat, ToneCurve, TonemapOperator};

/// Rec.709 luma 系数
const LR: f32 = 0.2126;
const LG: f32 = 0.7152;
const LB: f32 = 0.0722;
/// scRGB 线性光的绝对基准：1.0 = 80 nit（Windows scRGB 语义，与显示器 SDR 白设置无关）
const SCRGB_NITS_PER_UNIT: f32 = 80.0;
/// luma 缩放的除零保护阈值
const LUMA_EPS: f32 = 1e-6;

/// HDR → SDR 处理参数（v2：三段亮度语义拆分 + 算子专属参数平铺）
#[derive(Clone, Debug)]
pub struct HdrToSdrParams {
    pub operator: TonemapOperator,
    /// 输入 SDR 白（nits）：显示器实际读数（捕获路径）/ 80（文件解码路径）。
    /// 仅作 y_rel 归一化基准，不是输出 PNG 的物理峰值
    pub input_sdr_white_nits: f32,
    /// HDR 内容假定峰值（nits）：tone map 的源峰值
    pub source_peak_nits: f32,
    /// 输出 diffuse white（0..1）：SDR 白在 PNG 中的落点，1 − d 为高光 headroom
    pub output_diffuse_white: f32,
    /// 曝光档（EV，线性域 2^ev 作用于 RGB）
    pub exposure_ev: f32,
    /// 饱和度补偿（tone map 天然降饱和 2-5%，默认 1.02 微量回补）
    pub saturation: f32,
    /// 编码域 pivot 对比度（1.0 = 恒等）
    pub contrast: f32,
    /// 中性轴色域压缩强度（0 = 关闭仅 clamp，1 = 完全入域）
    pub gamut_strength: f32,
    /// 8bit 量化抖动（消除渐变色带）
    pub dither: bool,
    /// 智能自适应源峰值（99.9 分位 ±30% 钳制 + EMA 平滑，见 §7）
    pub adaptive_peak: bool,
    /// SmoothKnee 线性域拐点（仅 SmoothKnee 算子使用）
    pub knee_start: f32,
}

impl Default for HdrToSdrParams {
    fn default() -> Self {
        Self {
            operator: TonemapOperator::Bt2390,
            input_sdr_white_nits: 80.0,
            source_peak_nits: 1000.0,
            output_diffuse_white: 0.75,
            exposure_ev: 0.0,
            saturation: 1.02,
            contrast: 1.0,
            gamut_strength: 0.8,
            dither: true,
            adaptive_peak: false,
            knee_start: 0.9,
        }
    }
}

/// SDR RGBA8 图像（行优先，从左上角开始）
#[derive(Clone)]
pub struct SdrImage {
    pub width: u32,
    pub height: u32,
    /// RGBA8 像素数据（每像素 4 字节，已应用 sRGB 传递函数）
    pub rgba: Vec<u8>,
}

impl SdrImage {
    pub fn row_bytes(&self) -> usize {
        self.width as usize * 4
    }
}

/// 将捕获的纹理转换为 SDR RGBA8 图像（v2 完整管线）
pub fn to_sdr(captured: &CapturedTexture, params: &HdrToSdrParams) -> SdrImage {
    let w = captured.width;
    let h = captured.height;
    let mut rgba = vec![0u8; (w * h * 4) as usize];

    // SDR 输入（BGRA8 / GDI 兜底）：无 HDR 处理，直接通道交换
    // （内容已是 sRGB 编码，tone map / 抖动均不适用）
    if captured.format == PixelFormat::Bgra8 {
        process_bgra8(captured, &mut rgba);
        return SdrImage {
            width: w,
            height: h,
            rgba,
        };
    }

    // 智能自适应源峰值（仅 HDR 输入有意义）
    let mut source_peak = params.source_peak_nits;
    if params.adaptive_peak {
        source_peak = adaptive_source_peak(captured, source_peak);
    }

    // 每帧构造一次曲线（参数清洗 + 预计算）
    let curve = ToneCurve::new(
        params.operator,
        params.input_sdr_white_nits,
        source_peak,
        params.output_diffuse_white,
        params.knee_start,
    );

    match captured.format {
        PixelFormat::R16g16b16a16Float => process_scrgb(captured, &mut rgba, params, &curve),
        PixelFormat::R10g10b10a2 => process_hdr10(captured, &mut rgba, params, &curve),
        PixelFormat::Bgra8 => unreachable!(),
    }

    SdrImage {
        width: w,
        height: h,
        rgba,
    }
}

/// SDR: BGRA → RGBA 直拷贝（sRGB 编码原样保留）
fn process_bgra8(captured: &CapturedTexture, rgba: &mut [u8]) {
    let bpp = 4;
    for y in 0..captured.height as usize {
        let src_off = y * captured.row_pitch;
        let dst_off = y * captured.width as usize * bpp;
        for x in 0..captured.width as usize {
            let s = src_off + x * bpp;
            let d = dst_off + x * bpp;
            rgba[d] = captured.data[s + 2]; // R
            rgba[d + 1] = captured.data[s + 1]; // G
            rgba[d + 2] = captured.data[s]; // B
            rgba[d + 3] = captured.data[s + 3]; // A
        }
    }
}

/// scRGB FP16 → SDR：scRGB 1.0 = 80 nit 绝对语义
fn process_scrgb(
    captured: &CapturedTexture,
    rgba: &mut [u8],
    params: &HdrToSdrParams,
    curve: &ToneCurve,
) {
    let row_pitch = captured.row_pitch;
    let width = captured.width as usize;
    let exposure = 2.0f32.powf(params.exposure_ev.clamp(-2.0, 2.0));
    let px = map_hdr_pixel_fn(params, curve, exposure);

    for y in 0..captured.height as usize {
        let src_off = y * row_pitch;
        let dst_off = y * width * 4;
        for x in 0..width {
            let s = src_off + x * 8; // 4 × f16 = 8 bytes
            let r = sanitize(f16_to_f32(
                captured.data[s..s + 2].try_into().unwrap_or([0; 2]),
            ));
            let g = sanitize(f16_to_f32(
                captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]),
            ));
            let b = sanitize(f16_to_f32(
                captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]),
            ));
            // scRGB → 绝对 nits（1.0 = 80 nit）
            px(
                r * SCRGB_NITS_PER_UNIT,
                g * SCRGB_NITS_PER_UNIT,
                b * SCRGB_NITS_PER_UNIT,
                x,
                y,
                &mut rgba[dst_off + x * 4..dst_off + x * 4 + 4],
            );
        }
    }

    // 中心像素诊断（sidecar 之外的关键现场记录）
    let cx = width / 2;
    let cy = captured.height as usize / 2;
    let s = cy * row_pitch + cx * 8;
    if s + 6 <= captured.data.len() {
        let r = f16_to_f32(captured.data[s..s + 2].try_into().unwrap_or([0; 2]));
        let g = f16_to_f32(captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
        let b = f16_to_f32(captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
        let y_rel =
            (LR * r + LG * g + LB * b) * SCRGB_NITS_PER_UNIT / params.input_sdr_white_nits.max(1.0);
        log::info!(
            "SDR 转换诊断: op={:?} white={:.0}nits peak={:.0}nits d={} ev={} sat={} 像素({},{}) scRGB=({:.2},{:.2},{:.2}) y_rel={:.2}→{} sRGB=({},{},{})",
            params.operator,
            params.input_sdr_white_nits,
            params.source_peak_nits,
            params.output_diffuse_white,
            params.exposure_ev,
            params.saturation,
            cx,
            cy,
            r,
            g,
            b,
            y_rel,
            curve.map(y_rel),
            rgba[(cy * width + cx) * 4],
            rgba[(cy * width + cx) * 4 + 1],
            rgba[(cy * width + cx) * 4 + 2],
        );
    }
}

/// HDR10 (R10G10B10A2, PQ, BT.2020) → SDR：PQ EOTF → 绝对 nits → BT.2020→709
fn process_hdr10(
    captured: &CapturedTexture,
    rgba: &mut [u8],
    params: &HdrToSdrParams,
    curve: &ToneCurve,
) {
    let gamut = bt2020_to_bt709();
    let row_pitch = captured.row_pitch;
    let width = captured.width as usize;
    let exposure = 2.0f32.powf(params.exposure_ev.clamp(-2.0, 2.0));
    let px = map_hdr_pixel_fn(params, curve, exposure);

    for y in 0..captured.height as usize {
        let src_off = y * row_pitch;
        let dst_off = y * width * 4;
        for x in 0..width {
            let s = src_off + x * 4;
            let packed = u32::from_le_bytes([
                captured.data[s],
                captured.data[s + 1],
                captured.data[s + 2],
                captured.data[s + 3],
            ]);
            let r10 = packed & 0x3FF;
            let g10 = (packed >> 10) & 0x3FF;
            let b10 = (packed >> 20) & 0x3FF;
            // PQ 解码 → 绝对 nits（线性光 1.0 = 10000 nit）
            let r = pq_eotf(r10 as f32 / 1023.0) * 10000.0;
            let g = pq_eotf(g10 as f32 / 1023.0) * 10000.0;
            let b = pq_eotf(b10 as f32 / 1023.0) * 10000.0;
            // BT.2020 → BT.709（线性域矩阵对 nits 同样成立）
            let (r, g, b) = gamut.apply(sanitize(r), sanitize(g), sanitize(b));
            px(
                r,
                g,
                b,
                x,
                y,
                &mut rgba[dst_off + x * 4..dst_off + x * 4 + 4],
            );
        }
    }
}

/// 构造逐像素处理闭包（曝光 → luma tone map → 等比缩放 → 饱和 → 色域压缩 → OETF → 对比度 → 量化）
fn map_hdr_pixel_fn<'a>(
    params: &'a HdrToSdrParams,
    curve: &'a ToneCurve,
    exposure: f32,
) -> impl Fn(f32, f32, f32, usize, usize, &mut [u8]) + 'a {
    let white = params.input_sdr_white_nits.max(1.0);
    let saturation = params.saturation;
    let contrast = params.contrast;
    let g_strength = params.gamut_strength.clamp(0.0, 1.0);
    let dither = params.dither;
    move |r_nits, g_nits, b_nits, x, y, out| {
        // 曝光（线性域）→ 归一化到 y_rel 域（1.0 = 输入 SDR 白）
        let r = r_nits * exposure / white;
        let g = g_nits * exposure / white;
        let b = b_nits * exposure / white;

        // 亮度驱动 tone map + RGB 等比缩放（保线性色度比例）
        let y_rel = LR * r + LG * g + LB * b;
        let y_out = if y_rel > 0.0 { curve.map(y_rel) } else { 0.0 };
        let scale = if y_rel > LUMA_EPS {
            y_out / y_rel
        } else {
            // Y≈0：近黑彩色像素，按曲线输出等比压暗（避免除零放大噪声）
            y_out / LUMA_EPS
        };
        let mut r = r * scale;
        let mut g = g * scale;
        let mut b = b * scale;

        // 饱和度（围绕 luma 混合，luma 不变）
        if (saturation - 1.0).abs() > 1e-4 {
            let l = LR * r + LG * g + LB * b;
            r = l + (r - l) * saturation;
            g = l + (g - l) * saturation;
            b = l + (b - l) * saturation;
        }

        // 中性轴色域压缩（正负通道一并处理）
        neutral_axis_compress(&mut r, &mut g, &mut b, g_strength);

        // 安全 clamp（仅浮点误差与极端值；大部分越界已由软压缩解决）
        let s_r = srgb_oetf(r.clamp(0.0, 1.0));
        let s_g = srgb_oetf(g.clamp(0.0, 1.0));
        let s_b = srgb_oetf(b.clamp(0.0, 1.0));

        // 编码域 pivot 对比度（1.0 恒等；<1 减弱；>1 增强）
        let (s_r, s_g, s_b) = if (contrast - 1.0).abs() > 1e-4 {
            let c = contrast;
            (
                (0.5 + c * (s_r - 0.5)).clamp(0.0, 1.0),
                (0.5 + c * (s_g - 0.5)).clamp(0.0, 1.0),
                (0.5 + c * (s_b - 0.5)).clamp(0.0, 1.0),
            )
        } else {
            (s_r, s_g, s_b)
        };

        // 量化（零均值 Bayer 抖动；alpha 固定不透明不抖动）
        let xi = x as u32;
        let yi = y as u32;
        out[0] = if dither {
            srgb8_quantize_dithered(s_r, xi, yi)
        } else {
            srgb8_quantize(s_r)
        };
        out[1] = if dither {
            srgb8_quantize_dithered(s_g, xi, yi)
        } else {
            srgb8_quantize(s_g)
        };
        out[2] = if dither {
            srgb8_quantize_dithered(s_b, xi, yi)
        } else {
            srgb8_quantize(s_b)
        };
        out[3] = 255;
    }
}

/// 中性轴色域压缩（v2 文档 §3.6 / 附录 B）
///
/// `RGB' = Y + k·(RGB − Y)`，k 取使所有通道落入 [0,1] 的最大值 k_max，
/// 再按 `strength` 在 1 与 k_max 之间插值（0=不压缩，1=完全入域）。
/// 同时处理正通道越界（>1）与负通道越界（<0），不改变 luma（luma > 1 时
/// 先等比压暗到 1——luma 本身越界时色度压缩无法修复，只能整体降亮度）。
#[inline]
fn neutral_axis_compress(r: &mut f32, g: &mut f32, b: &mut f32, strength: f32) {
    let mut y = LR * *r + LG * *g + LB * *b;
    // 病态输入（负 luma，仅合成数据会出现）：中性轴无意义，直接硬 clamp
    if y <= 0.0 {
        *r = r.clamp(0.0, 1.0);
        *g = g.clamp(0.0, 1.0);
        *b = b.clamp(0.0, 1.0);
        return;
    }
    // luma 本身超过纸白：等比缩暗到 1（保持色度比例）
    if y > 1.0 {
        let s = 1.0 / y;
        *r *= s;
        *g *= s;
        *b *= s;
        y = 1.0;
    }
    let mut k_max = 1.0f32;
    for &c in &[*r, *g, *b] {
        if c > y && y <= 1.0 {
            // 上界：C' = Y + k(C−Y) ≤ 1 → k ≤ (1−Y)/(C−Y)
            // （y = 1 时上界为 0：luma 已到纸白，通道越界只能全坍缩为灰）
            k_max = k_max.min((1.0 - y) / (c - y));
        } else if c < y && y > 0.0 {
            // 下界：C' = Y + k(C−Y) ≥ 0 → k ≤ Y/(Y−C)
            k_max = k_max.min(y / (y - c));
        }
    }
    let k = 1.0 - (1.0 - k_max.clamp(0.0, 1.0)) * strength;
    *r = y + (*r - y) * k;
    *g = y + (*g - y) * k;
    *b = y + (*b - y) * k;
}

/// 输入清洗：NaN/Inf → 0（v2 文档 §3.11）
#[inline]
fn sanitize(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

// ==================== 智能自适应源峰值（v2 文档 §7） ====================

/// EMA 平滑后的自适应峰值（f32 位模式；0 = 未初始化）
static ADAPTIVE_PEAK_SMOOTHED: AtomicU32 = AtomicU32::new(0);
/// 直方图 bin 数（线性域覆盖 0..12000 nits）
const ADAPTIVE_BINS: usize = 1024;
const ADAPTIVE_MAX_NITS: f32 = 12000.0;
/// 采样上限（控制分析开销 ≈ 单遍 1M 像素）
const ADAPTIVE_MAX_SAMPLES: usize = 1_000_000;

/// 分析捕获帧亮度分布，推导自适应源峰值：
/// 99.9 分位（免疫鼠标指针/孤立高光）→ ±30% 钳制（防突变）→ EMA 平滑（帧间稳定）
fn adaptive_source_peak(tex: &CapturedTexture, base_peak: f32) -> f32 {
    let base = if base_peak.is_finite() && base_peak > 0.0 {
        base_peak
    } else {
        1000.0
    };
    let mut hist = [0u32; ADAPTIVE_BINS];
    let mut total: u64 = 0;

    let w = tex.width as usize;
    let h = tex.height as usize;
    if w == 0 || h == 0 {
        return base;
    }
    // 采样步长：限制总样本数
    let step = ((w * h) / ADAPTIVE_MAX_SAMPLES).max(1);

    let bin_of = |nits: f32| -> usize {
        if nits <= 0.0 || !nits.is_finite() {
            usize::MAX // 负值/异常不入直方图
        } else {
            (((nits / ADAPTIVE_MAX_NITS) * ADAPTIVE_BINS as f32) as usize).min(ADAPTIVE_BINS - 1)
        }
    };

    match tex.format {
        PixelFormat::R16g16b16a16Float => {
            for y in 0..h {
                let src_off = y * tex.row_pitch;
                let mut x = 0;
                while x < w {
                    let s = src_off + x * 8;
                    if s + 6 <= tex.data.len() {
                        let r = f16_to_f32(tex.data[s..s + 2].try_into().unwrap_or([0; 2]));
                        let g = f16_to_f32(tex.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
                        let b = f16_to_f32(tex.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
                        let bin = bin_of((LR * r + LG * g + LB * b) * SCRGB_NITS_PER_UNIT);
                        if bin != usize::MAX {
                            hist[bin] += 1;
                            total += 1;
                        }
                    }
                    x += step;
                }
            }
        }
        PixelFormat::R10g10b10a2 => {
            for y in 0..h {
                let src_off = y * tex.row_pitch;
                let mut x = 0;
                while x < w {
                    let s = src_off + x * 4;
                    if s + 4 <= tex.data.len() {
                        let packed = u32::from_le_bytes([
                            tex.data[s],
                            tex.data[s + 1],
                            tex.data[s + 2],
                            tex.data[s + 3],
                        ]);
                        let r = pq_eotf((packed & 0x3FF) as f32 / 1023.0);
                        let g = pq_eotf(((packed >> 10) & 0x3FF) as f32 / 1023.0);
                        let b = pq_eotf(((packed >> 20) & 0x3FF) as f32 / 1023.0);
                        let bin = bin_of((LR * r + LG * g + LB * b) * 10000.0);
                        if bin != usize::MAX {
                            hist[bin] += 1;
                            total += 1;
                        }
                    }
                    x += step;
                }
            }
        }
        PixelFormat::Bgra8 => return base, // SDR 输入无自适应意义
    }

    if total == 0 {
        return base;
    }

    // 99.9 分位
    let target = ((total as f64) * 0.999).ceil() as u64;
    let mut acc: u64 = 0;
    let mut p999 = base;
    for (i, &c) in hist.iter().enumerate() {
        acc += c as u64;
        if acc >= target {
            p999 = (i as f32 + 1.0) / ADAPTIVE_BINS as f32 * ADAPTIVE_MAX_NITS;
            break;
        }
    }

    // ±30% 钳制 + EMA 平滑（首帧直接采用）
    let clamped = p999.clamp(base * 0.7, base * 1.3);
    let prev_bits = ADAPTIVE_PEAK_SMOOTHED.load(Ordering::Relaxed);
    let smoothed = if prev_bits == 0 {
        clamped
    } else {
        f32::from_bits(prev_bits) * 0.7 + clamped * 0.3
    };
    ADAPTIVE_PEAK_SMOOTHED.store(smoothed.to_bits(), Ordering::Relaxed);
    log::info!(
        "智能峰值: P99.9={:.0}nits → 钳制[{:.0},{:.0}]→{:.0} → EMA 平滑→{:.0}（基准 {:.0}）",
        p999,
        base * 0.7,
        base * 1.3,
        clamped,
        smoothed,
        base
    );
    smoothed
}

/// IEEE 754 半精度浮点 → f32
pub fn f16_to_f32(bytes: [u8; 2]) -> f32 {
    let bits = u16::from_le_bytes(bytes);
    let sign = (bits >> 15) & 1;
    let exp = (bits >> 10) & 0x1f;
    let mant = bits & 0x3ff;

    let val = if exp == 0 {
        if mant == 0 {
            0.0
        } else {
            // 非正规数：mant × 2^-24
            (mant as f32) * (1.0 / (1024.0 * 16384.0))
        }
    } else if exp == 0x1f {
        if mant == 0 {
            f32::INFINITY
        } else {
            f32::NAN
        }
    } else {
        let e = exp as i32 - 15;
        let m = 1.0 + (mant as f32) / 1024.0;
        m * 2f32.powi(e)
    };

    if sign == 1 {
        -val
    } else {
        val
    }
}

/// 提取 scRGB 原始 FP16 数据为 f32 线性光（用于 EXR 导出）
pub fn extract_scrgb_linear(captured: &CapturedTexture) -> (u32, u32, Vec<f32>) {
    let w = captured.width;
    let h = captured.height;
    let mut out = vec![0f32; (w * h * 3) as usize];
    let row_pitch = captured.row_pitch;

    for y in 0..h as usize {
        for x in 0..w as usize {
            let s = y * row_pitch + x * 8;
            let r = f16_to_f32(captured.data[s..s + 2].try_into().unwrap_or([0; 2]));
            let g = f16_to_f32(captured.data[s + 2..s + 4].try_into().unwrap_or([0; 2]));
            let b = f16_to_f32(captured.data[s + 4..s + 6].try_into().unwrap_or([0; 2]));
            let off = (y * w as usize + x) * 3;
            out[off] = r;
            out[off + 1] = g;
            out[off + 2] = b;
        }
    }
    (w, h, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrgb_texture(w: u32, h: u32, fill: impl Fn(usize, usize) -> [f32; 3]) -> CapturedTexture {
        let mut data = vec![0u8; (w * h * 8) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let [r, g, b] = fill(x, y);
                let off = (y * w as usize + x) * 8;
                data[off..off + 8].copy_from_slice(&f32_to_f16_bytes(r, g, b));
            }
        }
        CapturedTexture {
            width: w,
            height: h,
            format: PixelFormat::R16g16b16a16Float,
            data,
            row_pitch: w as usize * 8,
            via_gdi: false,
        }
    }

    fn f32_to_f16_bytes(r: f32, g: f32, b: f32) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..2].copy_from_slice(&f32_to_f16(r).to_le_bytes());
        out[2..4].copy_from_slice(&f32_to_f16(g).to_le_bytes());
        out[4..6].copy_from_slice(&f32_to_f16(b).to_le_bytes());
        out[6..8].copy_from_slice(&0x3C00u16.to_le_bytes()); // A = 1.0
        out
    }

    fn f32_to_f16(v: f32) -> u16 {
        // 测试辅助：只覆盖 0..~32 的正数（指数 15 附近），完整转换由 f16_to_f32 反向验证
        if v == 0.0 {
            return 0;
        }
        let bits = v.to_bits();
        let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        let mant = (bits & 0x7f_ffff) >> 13;
        ((exp as u16) << 10) | mant as u16
    }

    #[test]
    fn f16_known_values() {
        // 1.0 in f16 = 0x3C00
        assert!((f16_to_f32([0x00, 0x3C]) - 1.0).abs() < 1e-6);
        // 0.0
        assert_eq!(f16_to_f32([0, 0]), 0.0);
        // 2.0 = 0x4000
        assert!((f16_to_f32([0x00, 0x40]) - 2.0).abs() < 1e-6);
        // 0.5 = 0x3800
        assert!((f16_to_f32([0x00, 0x38]) - 0.5).abs() < 1e-6);
        // NaN
        assert!(f16_to_f32([0xff, 0x7e]).is_nan());
    }

    #[test]
    fn gamut_compression_contains() {
        // 正通道越界（高饱和红）+ 负通道（BT.2020 矩阵产物），strength=1 完全入域
        let cases = [
            (1.5f32, 0.2, 0.1),
            (-0.3, 0.8, 0.4),
            (2.0, 1.8, 1.9),
            (-0.5, -0.4, 0.9),
            (1.2, -0.2, 0.05),
        ];
        for (mut r, mut g, mut b) in cases {
            neutral_axis_compress(&mut r, &mut g, &mut b, 1.0);
            assert!((0.0..=1.0).contains(&r), "R 越界: {}", r);
            assert!((0.0..=1.0).contains(&g), "G 越界: {}", g);
            assert!((0.0..=1.0).contains(&b), "B 越界: {}", b);
        }
        // 灰阶保持灰阶（中性轴缩放不动中性色）
        let (mut r, mut g, mut b) = (0.6, 0.6, 0.6);
        neutral_axis_compress(&mut r, &mut g, &mut b, 1.0);
        assert!((r - 0.6).abs() < 1e-6 && (g - 0.6).abs() < 1e-6 && (b - 0.6).abs() < 1e-6);
        // strength=0：不压缩（仅后续 clamp）
        let (mut r, mut g, mut b) = (1.5, 0.2, 0.1);
        neutral_axis_compress(&mut r, &mut g, &mut b, 0.0);
        assert!((r - 1.5).abs() < 1e-6);
    }

    #[test]
    fn to_sdr_sdr_white_and_black() {
        // 白 = 200 nits（scRGB 2.5），input_sdr_white = 200 → y_rel = 1
        let tex = scrgb_texture(8, 8, |x, y| {
            if x == 0 && y == 0 {
                [0.0, 0.0, 0.0] // 黑
            } else {
                [2.5, 2.5, 2.5] // SDR 白
            }
        });
        let params = HdrToSdrParams {
            input_sdr_white_nits: 200.0,
            operator: TonemapOperator::Reinhard, // 线性段语义最可预测
            ..Default::default()
        };
        let img = to_sdr(&tex, &params);
        // 黑 → 0
        assert_eq!(img.rgba[0], 0);
        assert_eq!(img.rgba[1], 0);
        assert_eq!(img.rgba[2], 0);
        assert_eq!(img.rgba[3], 255);
        // SDR 白 → diffuse_out = 0.75 线性 → sRGB ≈ 0.88 → ≈ 224（±2 容差含抖动）
        let px = &img.rgba[4..8];
        assert!((px[0] as i32 - 224).abs() <= 2, "SDR 白落点: {:?}", px);
        assert_eq!(px[3], 255);
        // 灰阶保持灰阶
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);
    }

    #[test]
    fn to_sdr_all_outputs_bounded() {
        // 随机极端 HDR 输入：输出必须有限且在 0..255
        let tex = scrgb_texture(16, 16, |x, y| {
            let v = (x * y) as f32 * 0.37; // 0..~80（0..6080 nits）
            [v, v * 0.5 + 3.0, -v * 0.2] // 含负通道
        });
        for op in [
            TonemapOperator::Bt2390,
            TonemapOperator::SmoothKnee,
            TonemapOperator::Reinhard,
            TonemapOperator::Aces,
        ] {
            let params = HdrToSdrParams {
                operator: op,
                input_sdr_white_nits: 200.0,
                saturation: 1.3,
                contrast: 1.1,
                ..Default::default()
            };
            let img = to_sdr(&tex, &params);
            for (i, &v) in img.rgba.iter().enumerate() {
                assert!(v <= 255, "{:?} 输出超界 @{}: {}", op, i, v);
            }
        }
    }

    #[test]
    fn scrgb_80nit_semantics() {
        // scRGB 1.0 = 80 nit：input_sdr_white=80 时 y_rel(1.0)=1（SDR 白）
        // Reinhard 线性段 → 0.75 线性 → ≈224
        let tex = scrgb_texture(4, 4, |_, _| [1.0, 1.0, 1.0]);
        let params = HdrToSdrParams {
            operator: TonemapOperator::Reinhard,
            input_sdr_white_nits: 80.0,
            ..Default::default()
        };
        let img = to_sdr(&tex, &params);
        assert!(
            (img.rgba[0] as i32 - 224).abs() <= 2,
            "scRGB 1.0 应为 80nit SDR 白: {}",
            img.rgba[0]
        );
    }
}
