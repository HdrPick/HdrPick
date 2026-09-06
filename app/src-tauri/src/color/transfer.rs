//! 传递函数（Transfer Functions）
//!
//! 实现各色彩编码的 EOTF（信号→线性光）与 OETF（线性光→信号）。
//!
//! - PQ (SMPTE ST 2084)：HDR10 / scRGB HDR 使用，线性光 1.0 = 10000 nits
//! - HLG (BT.2100)：BBC/NHK HDR 标准
//! - sRGB：标准显示器
//! - BT.1886：SDR 显示器 gamma 2.4

/// PQ ST.2084 常量
const PQ_M1: f32 = 0.1593017578125; // 2610/16384
const PQ_M2: f32 = 78.84375; // 2523/4096 * 128
const PQ_C1: f32 = 0.8359375; // 2392/4096 * 32
const PQ_C2: f32 = 18.8515625; // 2413/4096 * 32
const PQ_C3: f32 = 18.6875; // 2693/4096 * 32

/// PQ 编码值（0..1）→ 线性光（0..1，1.0 = 10000 nits）
#[inline]
pub fn pq_eotf(e: f32) -> f32 {
    if e <= 0.0 {
        return 0.0;
    }
    let e_pos = e.min(1.0);
    let em = e_pos.powf(1.0 / PQ_M2);
    let num = (em - PQ_C1).max(0.0);
    let den = (PQ_C2 - PQ_C3 * em).max(1e-10);
    (num / den).powf(1.0 / PQ_M1)
}

/// 线性光（0..1）→ PQ 编码值（0..1）
#[inline]
pub fn pq_oetf(l: f32) -> f32 {
    if l <= 0.0 {
        return 0.0;
    }
    let l_pos = l.min(1.0);
    let lm = l_pos.powf(PQ_M1);
    // 必须先除再 powf，否则分子分母分别 powf(PQ_M2=78.8) 会溢出 f32 → +inf/+inf = NaN
    let np = (PQ_C1 + PQ_C2 * lm) / (1.0 + PQ_C3 * lm);
    np.powf(PQ_M2)
}

/// HLG 逆 OETF（信号→线性光，0..1 表示相对亮度）
/// HLG OETF^-1：对于 e<=1 使用 sqrt(3)*e^(0.5)；否则用对数
#[inline]
pub fn hlg_eotf(e: f32) -> f32 {
    if e <= 0.0 {
        0.0
    } else if e <= 0.5 {
        (e * e) / 3.0
    } else {
        (e.exp() - 0.02845) / 5.975
    }
}

/// sRGB EOTF（信号→线性光）
#[inline]
pub fn srgb_eotf(e: f32) -> f32 {
    if e <= 0.04045 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB OETF（线性光→信号）
#[inline]
pub fn srgb_oetf(l: f32) -> f32 {
    if l <= 0.0031308 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// BT.1886 EOTF（SDR 显示器 gamma 2.4，信号→线性光）
#[inline]
pub fn bt1886_eotf(e: f32, l_black: f32, l_white: f32) -> f32 {
    if e <= 0.0 {
        return 0.0;
    }
    let a = (l_white.powf(1.0 / 2.4) - l_black.powf(1.0 / 2.4)).powf(2.4);
    (e.max(l_black.powf(1.0 / 2.4)) / (l_white.powf(1.0 / 2.4) - l_black.powf(1.0 / 2.4))).powf(2.4)
        * a
}

/// 线性光 → sRGB 8bit
#[inline]
pub fn linear_to_srgb8(l: f32) -> u8 {
    let s = srgb_oetf(l.clamp(0.0, 1.0));
    (s * 255.0 + 0.5) as u8
}

/// 线性光 → sRGB 16bit
#[inline]
pub fn linear_to_srgb16(l: f32) -> u16 {
    let s = srgb_oetf(l.clamp(0.0, 1.0));
    (s * 65535.0 + 0.5) as u16
}

/// 4×4 Bayer 有序抖动矩阵（v2 文档 §3.10：消除 8bit 渐变色带）
const BAYER_4X4: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];

/// 已编码 sRGB 值（0..1）→ 8bit 量化（带零均值 Bayer 抖动）
///
/// 抖动偏移 d = (B + 0.5)/16 − 0.5（完整 tile 均值为 0，无系统性偏暗）；
/// 黑点/白点经 clamp 不溢出；alpha 不抖动（调用方负责）。
#[inline]
pub fn srgb8_quantize_dithered(s: f32, x: u32, y: u32) -> u8 {
    let idx = ((x & 3) + ((y & 3) << 2)) as usize;
    let d = (BAYER_4X4[idx] as f32 + 0.5) / 16.0 - 0.5;
    (s * 255.0 + d).round().clamp(0.0, 255.0) as u8
}

/// 已编码 sRGB 值（0..1）→ 8bit 量化（不抖动，严格 round）
#[inline]
pub fn srgb8_quantize(s: f32) -> u8 {
    (s.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_roundtrip() {
        for &v in &[0.1f32, 0.25, 0.5, 0.75, 0.9] {
            let enc = pq_oetf(v);
            let dec = pq_eotf(enc);
            assert!(
                (dec - v).abs() < 1e-4,
                "PQ roundtrip failed at {}: enc={} dec={}",
                v,
                enc,
                dec
            );
        }
    }

    #[test]
    fn srgb_roundtrip() {
        for &v in &[0.1f32, 0.25, 0.5, 0.75, 0.9] {
            let enc = srgb_oetf(v);
            let dec = srgb_eotf(enc);
            assert!(
                (dec - v).abs() < 1e-4,
                "sRGB roundtrip failed at {}: enc={} dec={}",
                v,
                enc,
                dec
            );
        }
    }

    #[test]
    fn pq_known_values() {
        // PQ 编码 0.5 ≈ 92 nits 左右
        let l = pq_eotf(0.5);
        assert!(l > 0.0 && l < 1.0, "PQ(0.5) should be in (0,1): {}", l);
        // PQ 编码 1.0 = 10000 nits = 线性 1.0
        assert!((pq_eotf(1.0) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn dither_zero_mean_and_bounds() {
        // 完整 4×4 tile 抖动偏移均值为 0（量化噪声无系统偏差）
        let mut sum = 0.0f32;
        for y in 0..4u32 {
            for x in 0..4u32 {
                let q = srgb8_quantize_dithered(0.5, x, y);
                sum += q as f32;
            }
        }
        let mean = sum / 16.0;
        // 0.5 编码值 × 255 = 127.5，抖动后均值应非常接近 127.5
        assert!((mean - 127.5).abs() < 0.4, "抖动均值偏移: {}", mean);

        // 黑点/白点不溢出
        for y in 0..4u32 {
            for x in 0..4u32 {
                assert_eq!(srgb8_quantize_dithered(0.0, x, y), 0);
                assert_eq!(srgb8_quantize_dithered(1.0, x, y), 255);
            }
        }

        // 固定输入输出可重复
        assert_eq!(
            srgb8_quantize_dithered(0.42, 1, 2),
            srgb8_quantize_dithered(0.42, 1, 2)
        );

        // 关闭抖动（srgb8_quantize）严格 round
        assert_eq!(srgb8_quantize(0.5), 128);
        assert_eq!(srgb8_quantize(0.0), 0);
        assert_eq!(srgb8_quantize(1.0), 255);
    }
}
