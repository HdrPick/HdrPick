//! SDR→HDR 画质提升（逆色调映射 / inverse tone mapping）
//!
//! AI 放大把 SDR 图片装入 HDR 容器（JXL HDR / PNG HDR / EXR）时的增强管线：
//! - 中间调（≤自适应拐点）：严格 SDR 等效——白 = 系统 SDR 参考白（还原基准）
//! - 高光（>拐点）：Hermite C1 平滑扩展到 参考白×highlight_scale，
//!   亮度域扩展、色度保持（天空/光源/金属反光获得真 HDR 高光）
//! - 自适应拐点：直方图感知（低动态图自动退化为纯等效还原）
//! - 色相保持色域外推：HSV 只提 Chroma 至 BT.2020 边界安全比例（替代简单饱和度）
//! - 过曝区 bright-mask：双边滤波纹理扩散缓解死白「贴纸感」（曲线法上限内的缓解）
//!
//! 与截图路径的区别：截图（to_pq_rgb）数据已是系统渲染后的绝对物理光；
//! 本模块输入是 sRGB 编码值（SDR 文件语义），需主动赋予 HDR 亮度语义。

/// 默认扩展拐点（BT.709 线性域）：sRGB 编码 ≈0.735 以上开始扩展
const KNEE_DEFAULT: f32 = 0.5;

/// 拐点搜索范围（线性亮度域）
const KNEE_MIN: f32 = 0.3;
const KNEE_MAX: f32 = 0.6;

/// 判定「有高光内容」的最低亮度（全图最大 luma 低于此 → 退化为纯等效还原）
const HIGHLIGHT_FLOOR: f32 = 0.7;

/// 过曝掩码起点/终点（sRGB 编码域：max(R,G,B) ≥0.92 开始进入过曝）
const CLIP_START: f32 = 0.92;

/// 过曝区纹理扩散强度（相对增益，越小越「只是缓解」）
const BRIGHT_DETAIL_EPS: f32 = 0.03;

/// SDR→HDR 增强参数
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SdrToHdrParams {
    /// 参考白 nits（monitor::effective_sdr_white_nits）
    pub diffuse_white_nits: f32,
    /// 高光峰值 = 参考白 × 此倍数；1.0 = 关闭扩展（纯 SDR 等效）
    pub highlight_scale: f32,
    /// 色域外推目标（1.0 = 不变；Vivid 档 0.85 = Chroma 提到 BT.2020 边界 85%）
    pub gamut_extrapolate: f32,
}

impl Default for SdrToHdrParams {
    fn default() -> Self {
        SdrToHdrParams {
            diffuse_white_nits: 80.0,
            highlight_scale: 1.0,
            gamut_extrapolate: 1.0,
        }
    }
}

/// UI 档位（放大窗口「HDR 增强」下拉）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SdrToHdrPreset {
    /// 关闭：纯 SDR 等效装容器（白=参考白，无高光扩展/色域外推/过曝处理）
    #[default]
    Off,
    /// 自然（默认）：高光 2×参考白（200nits 屏 → 400nits 峰值）
    Natural,
    /// 鲜艳：高光 3×参考白 + 色域外推 0.85
    Vivid,
    /// AI 智能：ITM 网络推理（P9 阶段二；模型缺失时回落 Natural）
    Ai,
}

impl SdrToHdrPreset {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(SdrToHdrPreset::Off),
            "natural" => Some(SdrToHdrPreset::Natural),
            "vivid" => Some(SdrToHdrPreset::Vivid),
            "ai" => Some(SdrToHdrPreset::Ai),
            _ => None,
        }
    }

    pub fn params(self, diffuse_white_nits: f32) -> SdrToHdrParams {
        let (highlight_scale, gamut_extrapolate) = match self {
            SdrToHdrPreset::Off => (1.0, 1.0),
            SdrToHdrPreset::Natural => (2.0, 1.0),
            SdrToHdrPreset::Vivid => (3.0, 0.85),
            // AI 档不走曲线参数（网络推理输出 nits）；此 fallback 仅供
            // 模型缺失时回落 Natural，命令层会先检查模型存在性
            SdrToHdrPreset::Ai => (2.0, 1.0),
        };
        SdrToHdrParams {
            diffuse_white_nits,
            highlight_scale,
            gamut_extrapolate,
        }
    }

    /// AI 档模型文件是否就绪（models\itm\Ensemble_AGCM_LE.pth，HDRTVNet 官方权重）
    pub fn ai_model_ready() -> bool {
        crate::upscale::itm::default_model_path().exists()
    }
}

/// Hermite 段（C1 连续：h(knee)=knee·ref、h'(knee)=ref、h(1)=peak、h'(1)=0）
///
/// 顶部导数归零避免触顶后 PQ 量化截断产生色带
#[inline]
fn hermite(t: f32, h0: f32, m0: f32, h1: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    h0 * h00 + m0 * h10 + h1 * h01
}

/// 高光扩展系数（亮度域）：y 线性亮度 → nits 缩放系数
///
/// y ≤ knee：返回参考白（严格等效）；y > knee：Hermite 上探 peak
#[inline]
fn highlight_scale_at(y: f32, knee: f32, diffuse_white: f32, peak: f32) -> f32 {
    if peak <= diffuse_white || y <= knee {
        return diffuse_white;
    }
    let t = ((y - knee) / (1.0 - knee)).clamp(0.0, 1.0);
    let y_h = hermite(t, knee * diffuse_white, diffuse_white, peak);
    if y > 1e-6 {
        y_h / y
    } else {
        diffuse_white
    }
}

/// 自适应拐点：线性亮度直方图（64 bin）→ knee
///
/// 输入：sRGB 编码域 CHW 切片（np = h*w，RGB 三平面连续）
/// 高光占比越少 → 拐点越高（越保守）；全图无高光（max < HIGHLIGHT_FLOOR）
/// 或扩展关闭 → 返回 None（调用方按纯等效处理）
pub fn adaptive_knee(sdr: &[f32], highlight_scale: f32) -> Option<f32> {
    if highlight_scale <= 1.0 {
        return None;
    }
    let np = sdr.len() / 3;
    let n_bins = 64;
    let mut hist = vec![0u64; n_bins];
    let mut max_y = 0.0f32;
    for i in 0..np {
        let y = 0.2126 * sdr[i] + 0.7152 * sdr[np + i] + 0.0722 * sdr[2 * np + i];
        max_y = max_y.max(y);
        let bin = ((y.clamp(0.0, 1.0) * (n_bins - 1) as f32) as usize).min(n_bins - 1);
        hist[bin] += 1;
    }
    if max_y < HIGHLIGHT_FLOOR {
        return None; // 无高光内容 → 纯等效还原
    }
    let total = np as f32;
    // 高光占比 = 亮度 > HIGHLIGHT_FLOOR 的像素比例
    // （直接阈值比 P95 分位稳健：双峰分布下 P95 会落在黑区导致误判 100%）
    let mut hi_count = 0u64;
    for i in 0..np {
        let y = 0.2126 * sdr[i] + 0.7152 * sdr[np + i] + 0.0722 * sdr[2 * np + i];
        if y > HIGHLIGHT_FLOOR {
            hi_count += 1;
        }
    }
    let highlight_ratio = hi_count as f32 / total;
    // 高光占比 1% → knee=0.6（保守）；占比 ≥10% → knee=0.3（激进）
    let t = ((highlight_ratio - 0.01) / 0.09).clamp(0.0, 1.0);
    Some(KNEE_MAX - t * (KNEE_MAX - KNEE_MIN))
}

/// RGB→HSV（h ∈ [0,360)，s/v ∈ [0,1]）
#[inline]
fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let d = mx - mn;
    if d <= 1e-6 {
        return (0.0, 0.0, mx);
    }
    let (h, s) = if (mx - r).abs() <= 1e-6 {
        let h = (g - b) / d;
        (if h < 0.0 { h + 6.0 } else { h }, d / mx)
    } else if (mx - g).abs() <= 1e-6 {
        ((b - r) / d + 2.0, d / mx)
    } else {
        ((r - g) / d + 4.0, d / mx)
    };
    (h * 60.0, s, mx)
}

/// HSV→RGB
#[inline]
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let c = v * s;
    let hp = (h / 60.0).rem_euclid(6.0);
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r1 + m, g1 + m, b1 + m]
}

/// 色相保持色域外推：HSV 域 Chroma 提升
///
/// 线性 RGB 的 HSV 中 s=1 即 BT.709 边界（min 通道=0）。「外推到 BT.2020」
/// 数学上 = 允许 s>1（min 通道为负）——BT.2020 原色比 709 更饱和，
/// 709 边界色映射到 2020 容器内仍有余量，s>1 的负通道在
/// BT.709→BT.2020 矩阵变换后落回正域（编码端已有 clamp 10000nits 兜底）。
/// ratio = 外推强度（Vivid 0.85 ≈ 提到 BT.2020 余量的 85%）；
/// 外推量与 s 成正比（低饱和色几乎不动，防脏）。
#[inline]
fn gamut_boost(lin: [f32; 3], ratio: f32) -> [f32; 3] {
    if (ratio - 1.0).abs() <= 1e-4 {
        return lin;
    }
    let (h, s, v) = rgb_to_hsv(lin[0], lin[1], lin[2]);
    if s <= 1e-4 {
        return lin; // 无色度不动
    }
    const GAMUT_HEADROOM: f32 = 0.35; // s>1 的外推余量（硬上限 1.35）
    let boost = 1.0 + GAMUT_HEADROOM * ratio * s;
    let out = hsv_to_rgb(h, (s * boost).min(1.35), v);
    [out[0], out[1], out[2]]
}

/// 单像素：sRGB 编码值 [0,1]³ → HDR 线性亮度 nits（BT.709 原色域）
///
/// knee=None 表示纯等效还原（无高光扩展）；extrapolate=1.0 跳过色域外推
pub fn sdr_to_hdr_nits_at(
    r: f32,
    g: f32,
    b: f32,
    knee: Option<f32>,
    p: &SdrToHdrParams,
) -> [f32; 3] {
    let mut lin = [
        crate::color::srgb_eotf(r.clamp(0.0, 1.0)),
        crate::color::srgb_eotf(g.clamp(0.0, 1.0)),
        crate::color::srgb_eotf(b.clamp(0.0, 1.0)),
    ];

    // 色相保持色域外推（线性域；vivid 档生效）
    lin = gamut_boost(lin, p.gamut_extrapolate);

    // 高光扩展（亮度域，色度保持）：scale=nits
    let peak = p.diffuse_white_nits * p.highlight_scale;
    if peak <= p.diffuse_white_nits {
        return [
            lin[0] * p.diffuse_white_nits,
            lin[1] * p.diffuse_white_nits,
            lin[2] * p.diffuse_white_nits,
        ];
    }
    let knee = knee.unwrap_or(KNEE_DEFAULT);
    let y = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
    let scale = highlight_scale_at(y, knee, p.diffuse_white_nits, peak);
    [lin[0] * scale, lin[1] * scale, lin[2] * scale]
}

/// 单像素旧接口（固定拐点；保留兼容既有调用/单测）
pub fn sdr_to_hdr_nits(r: f32, g: f32, b: f32, p: &SdrToHdrParams) -> [f32; 3] {
    sdr_to_hdr_nits_at(r, g, b, Some(KNEE_DEFAULT), p)
}

/// 全图 SDR→HDR 增强管线（编码入口统一调用）
///
/// 输入：sdr = sRGB 编码域 CHW [0,1] 切片（UpscaleOutput.rgb.data），w/h 尺寸
/// 输出：HDR 线性 nits CHW（BT.709 原色域）——下游色域矩阵/PQ 直接消费
/// 内含：自适应拐点直方图统计 + 逐像素增强 + 过曝区纹理扩散
pub fn sdr_to_hdr_image(sdr: &[f32], w: usize, h: usize, params: &SdrToHdrParams) -> Vec<f32> {
    let knee = adaptive_knee(sdr, params.highlight_scale);
    let np = w * h;
    let mut out = vec![0.0f32; np * 3];
    for i in 0..np {
        let n = sdr_to_hdr_nits_at(sdr[i], sdr[np + i], sdr[2 * np + i], knee, params);
        out[i] = n[0];
        out[np + i] = n[1];
        out[2 * np + i] = n[2];
    }
    bright_detail_diffuse(&out, sdr, w, h, params)
}

/// 全图 bright-mask 双边滤波纹理扩散（过曝死白缓解）
///
/// rgb：线性 nits 域 CHW（应已过 sdr_to_hdr_nits_at）；sdr：sRGB 编码域 CHW。
/// 返回新 Vec（过曝区叠加微弱纹理起伏；natural/vivid 档生效，off 档原样克隆）。
/// 掩码取自 sRGB 域（smoothstep(CLIP_START,1,max)）；细节 = |luma−3×3 均值|
/// 从过曝边缘向内部迭代扩散 3 轮（近似双边滤波），最后 nits *= 1+ε·细节。
fn bright_detail_diffuse(
    rgb: &[f32],
    sdr: &[f32],
    w: usize,
    h: usize,
    params: &SdrToHdrParams,
) -> Vec<f32> {
    if params.highlight_scale <= 1.0 {
        return rgb.to_vec();
    }
    let np = sdr.len() / 3;
    let mut mask = vec![0.0f32; np];
    for i in 0..np {
        let m = sdr[i].max(sdr[np + i]).max(sdr[2 * np + i]);
        let t = ((m - CLIP_START) / (1.0 - CLIP_START)).clamp(0.0, 1.0);
        mask[i] = t * t * (3.0 - 2.0 * t);
    }
    if mask.iter().all(|&m| m <= 0.01) {
        return rgb.to_vec();
    }
    let luma = |i: usize| 0.2126 * sdr[i] + 0.7152 * sdr[np + i] + 0.0722 * sdr[2 * np + i];
    let mut detail = vec![0.0f32; np];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if mask[i] <= 0.01 {
                continue;
            }
            let (mut sum, mut cnt) = (0.0f32, 0u32);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (ny, nx) = (y as i32 + dy, x as i32 + dx);
                    if ny >= 0 && ny < h as i32 && nx >= 0 && nx < w as i32 {
                        sum += luma((ny as usize) * w + nx as usize);
                        cnt += 1;
                    }
                }
            }
            detail[i] = (luma(i) - sum / cnt as f32).abs();
        }
    }
    for _ in 0..3 {
        let mut next = detail.clone();
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if mask[i] <= 0.01 {
                    continue;
                }
                let (mut sum, mut cnt) = (0.0f32, 0u32);
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let (ny, nx) = (y as i32 + dy, x as i32 + dx);
                        if ny >= 0 && ny < h as i32 && nx >= 0 && nx < w as i32 {
                            sum += detail[(ny as usize) * w + nx as usize];
                            cnt += 1;
                        }
                    }
                }
                next[i] = detail[i] * 0.4 + (sum / cnt as f32) * 0.6;
            }
        }
        detail = next;
    }
    let mut out = rgb.to_vec();
    for i in 0..np {
        if mask[i] > 0.01 {
            let d = detail[i].clamp(0.0, 0.2) / 0.05;
            for c in 0..3 {
                out[c * np + i] *= 1.0 + BRIGHT_DETAIL_EPS * d;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(scale: f32) -> SdrToHdrParams {
        SdrToHdrParams {
            diffuse_white_nits: 200.0,
            highlight_scale: scale,
            gamut_extrapolate: 1.0,
        }
    }

    #[test]
    fn test_sdr_to_hdr() {
        // 关闭档：纯线性 ×参考白
        let out = sdr_to_hdr_nits(1.0, 1.0, 1.0, &p(1.0));
        assert!((out[0] - 200.0).abs() < 1e-4, "关闭档白=参考白: {}", out[0]);

        // 中间调（knee 以下）严格等效
        let a = sdr_to_hdr_nits(0.5, 0.5, 0.5, &p(2.0));
        let b = sdr_to_hdr_nits(0.5, 0.5, 0.5, &p(1.0));
        assert!((a[0] - b[0]).abs() < 1e-4, "中间调不受扩展影响");

        // 白 → peak（C1：h'(1)=0，触顶平缓）
        let w = sdr_to_hdr_nits(1.0, 1.0, 1.0, &p(2.0));
        assert!((w[0] - 400.0).abs() < 1e-3, "白扩展到 2×: {}", w[0]);

        // 色度保持：灰阶扩展后仍是灰阶
        let g = sdr_to_hdr_nits(0.9, 0.9, 0.9, &p(2.0));
        assert!(
            (g[0] - g[1]).abs() < 1e-4 && (g[1] - g[2]).abs() < 1e-4,
            "灰阶色度不变: {:?}",
            g
        );

        // 连续性：拐点两侧差值极小（C1 无跳变）
        let lo = sdr_to_hdr_nits(0.7349, 0.7349, 0.7349, &p(2.0)); // srgb(≈0.4999)
        let hi = sdr_to_hdr_nits(0.7351, 0.7351, 0.7351, &p(2.0));
        assert!(
            (lo[0] - hi[0]).abs() < 0.5,
            "拐点连续: {} vs {}",
            lo[0],
            hi[0]
        );
    }

    #[test]
    fn test_adaptive_knee() {
        // 无高光图（全 0.5）→ None → 纯等效
        let flat = vec![0.5f32; 64 * 3];
        assert!(adaptive_knee(&flat, 2.0).is_none(), "无高光退化为 None");

        // 关闭扩展 → None
        let mut hi = vec![0.0f32; 64 * 3];
        for i in 0..64 {
            hi[i] = 0.95;
            hi[64 + i] = 0.95;
            hi[128 + i] = 0.95;
        }
        assert!(
            adaptive_knee(&hi, 1.0).is_none(),
            "highlight_scale=1 → None"
        );

        // 有高光图 → Some，且高光占比大 → 拐点低（激进）
        let knee_dense = adaptive_knee(&hi, 2.0).expect("有高光应有拐点");
        // 全图都是 0.95 → 高光占比 100% → knee=KNEE_MIN
        assert!(
            (knee_dense - KNEE_MIN).abs() < 1e-4,
            "全高光 → 最激进拐点: {}",
            knee_dense
        );

        // 稀疏高光（2%）→ 拐点高（保守）
        let mut sparse = vec![0.0f32; 256 * 3];
        for i in 0..5 {
            sparse[i] = 0.95;
            sparse[256 + i] = 0.95;
            sparse[512 + i] = 0.95;
        }
        let knee_sparse = adaptive_knee(&sparse, 2.0).expect("有高光应有拐点");
        assert!(
            knee_sparse > knee_dense,
            "稀疏高光拐点应更保守: {} vs {}",
            knee_sparse,
            knee_dense
        );
    }

    #[test]
    fn test_gamut_extrapolate() {
        // 灰色不动
        let gray = gamut_boost([0.5, 0.5, 0.5], 0.85);
        assert!((gray[0] - 0.5).abs() < 1e-4, "灰不受外推: {:?}", gray);

        // 纯红外推：更饱和（g/b 更低），色相不变（仍是红轴）
        let red = gamut_boost([0.8, 0.1, 0.1], 0.85);
        assert!(red[0] >= 0.8 - 1e-4, "R 不降: {:?}", red);
        assert!(red[1] < 0.1 && red[2] < 0.1, "G/B 降低=更饱和: {:?}", red);
        // 色相验证：外推前后 hue 均为 0
        let (h0, _, _) = rgb_to_hsv(0.8, 0.1, 0.1);
        let (h1, _, _) = rgb_to_hsv(red[0], red[1], red[2]);
        assert!((h0 - h1).abs() < 1e-3, "色相严格不变: {} vs {}", h0, h1);

        // 低饱和色几乎不动（防脏）
        let low = gamut_boost([0.5, 0.45, 0.4], 0.85);
        assert!((low[0] - 0.5).abs() < 0.01, "低饱和色基本不变: {:?}", low);
    }

    #[test]
    fn test_bright_detail() {
        // 纯白图（全过曝）+ 一角有暗纹理 → 扩散后过曝区应有微弱起伏（方差>0）
        let mut sdr = vec![1.0f32; 256 * 3];
        // 角落放暗点（提供细节源）
        sdr[0] = 0.5;
        sdr[256] = 0.5;
        sdr[512] = 0.5;
        let out = sdr_to_hdr_image(&sdr, 16, 16, &SdrToHdrPreset::Natural.params(200.0));
        // 检查暗点邻近过曝区（扩散半径 ~4px 内）方差：(1..5)×(1..5) 全为过曝
        let np = 256;
        let mut vals = Vec::new();
        for y in 1..5 {
            for x in 1..5 {
                vals.push(out[y * 16 + x]);
            }
        }
        let mean = vals.iter().sum::<f32>() / vals.len() as f32;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / vals.len() as f32;
        assert!(var > 0.0, "过曝区应有微弱起伏（方差 {}）", var);
    }

    #[test]
    fn test_full_pipeline_off_equivalent() {
        // off 档 = 纯线性 ×参考白（全图，无过曝处理）
        let mut sdr = vec![0.0f32; 16 * 3];
        for i in 0..16 {
            sdr[i] = i as f32 / 16.0;
            sdr[16 + i] = 0.3;
            sdr[32 + i] = 0.6;
        }
        let out = sdr_to_hdr_image(&sdr, 4, 4, &SdrToHdrPreset::Off.params(200.0));
        for i in 0..16 {
            let e = crate::color::srgb_eotf(sdr[i]);
            assert!(
                (out[i] - e * 200.0).abs() < 1e-3,
                "off 档 = 纯等效: {} vs {}",
                out[i],
                e * 200.0
            );
        }
    }
}
