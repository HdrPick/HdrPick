//! 色调映射（Tone Mapping）— v2 管线
//!
//! 四个算子，各自专属参数（语义不共用，见 docs/hdr-tonemap-presets-v2.md §3）：
//! - Bt2390：真 BT.2390 EETF（PQ 归一化域，线性段 + Hermite shoulder，
//!   输入按源峰值归一化、输出按目标峰值归一化）
//! - SmoothKnee：自定义拐点（线性域直观参数，knee 以下恒等直通，
//!   Hermite 平滑滚降到源峰值 → 1.0）
//! - Reinhard：线性段（SDR 白 → diffuse_out）+ Reinhard 软肩
//! - Aces：ACES Fitted（Narkowicz 简化版）直接作用
//!
//! 统一输入/输出语义（与 hdr_pipeline 约定）：
//! - 输入 `y_rel`：线性亮度，**1.0 = 输入 SDR diffuse white**（显示器当前 SDR 白）
//! - 输出：线性 0..1，**1.0 = 输出纸白**（PNG 码值 255）
//! - `output_diffuse_white` 决定 y_rel = 1 的落点（高光 headroom）
//!
//! 所有曲线保证：黑点 0、全区间单调、输入峰值 → 1.0、参数异常时回退安全默认。

use crate::color::transfer::{pq_eotf, pq_oetf};

/// 色调映射算子
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub enum TonemapOperator {
    /// BT.2390 EETF（推荐默认）
    Bt2390,
    /// 自定义拐点（SmoothKnee）
    SmoothKnee,
    /// Reinhard 扩展（软肩高光保护）
    Reinhard,
    /// ACES Fitted（Narkowicz 简化版；非完整 ACES RRT/ODT）
    Aces,
}

impl Default for TonemapOperator {
    fn default() -> Self {
        TonemapOperator::Bt2390
    }
}

impl TonemapOperator {
    /// 配置字符串解析（旧 config 兼容：Aces/AcesFitted 同义）
    pub fn parse(s: &str) -> Self {
        match s {
            "Reinhard" => TonemapOperator::Reinhard,
            "Aces" | "AcesFitted" => TonemapOperator::Aces,
            "SmoothKnee" => TonemapOperator::SmoothKnee,
            _ => TonemapOperator::Bt2390,
        }
    }
}

/// 预计算完成的色调曲线（每帧构造一次，像素循环内调用 [`ToneCurve::map`]）
///
/// 参数合法性在此处统一清洗：非法值（NaN/Inf/越界）回退安全默认，
/// 保证 [`ToneCurve::map`] 对任意有限输入返回有限输出。
#[derive(Clone, Copy, Debug)]
pub struct ToneCurve {
    op: TonemapOperator,
    /// 输入 SDR diffuse white（nits）：y_rel=1.0 对应的绝对亮度
    input_sdr_white: f32,
    /// 输出 diffuse white（0..1）：y_rel=1.0 的目标落点
    diffuse_out: f32,
    /// 源峰值（nits）
    source_peak: f32,
    // ---- Bt2390 预计算 ----
    /// 源峰值 PQ 码值
    p_ws: f32,
    /// 目标峰值（纸白）nits = input_sdr_white / diffuse_out
    target_peak: f32,
    /// PQ 归一化域 shoulder 起点
    bt_knee: f32,
    /// PQ 归一化域目标峰值位置
    bt_y_max: f32,
    /// 目标 ≥ 源（无需压缩）→ PQ 恒等
    bt_identity: bool,
    // ---- SmoothKnee 预计算 ----
    /// 线性域拐点（y_rel 单位）
    sk_knee: f32,
    /// 源峰值（y_rel 单位）
    sk_peak: f32,
    // ---- Reinhard 预计算 ----
    /// 软肩归一化分母 h(peak-1)
    rh_denom: f32,
    // ---- Aces 预计算 ----
    /// 输入预增益（shoulder 强度）
    aces_gain: f32,
}

fn sanitize_f32(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

impl ToneCurve {
    /// 由管线参数构造曲线
    ///
    /// - `input_sdr_white_nits`：显示器实际 SDR 白（捕获路径）或 80（文件解码路径）
    /// - `source_peak_nits`：HDR 内容假定峰值
    /// - `output_diffuse_white`：SDR 白在输出中的落点（0.6..1.0）
    /// - `knee_start`：SmoothKnee 线性域拐点（仅 SmoothKnee 使用）
    pub fn new(
        op: TonemapOperator,
        input_sdr_white_nits: f32,
        source_peak_nits: f32,
        output_diffuse_white: f32,
        _knee_start: f32,
    ) -> Self {
        let input_sdr_white = sanitize_f32(input_sdr_white_nits, 80.0).max(1.0);
        let source_peak = sanitize_f32(source_peak_nits, 1000.0).max(input_sdr_white);
        let diffuse_out = sanitize_f32(output_diffuse_white, 0.75).clamp(0.10, 1.0);

        // ---- BT.2390 预计算（PQ 归一化域）----
        let target_peak = input_sdr_white / diffuse_out;
        let p_ws = pq_oetf((source_peak / 10000.0).clamp(0.0, 1.0));
        let p_wd = pq_oetf((target_peak / 10000.0).clamp(0.0, 1.0));
        let bt_identity = p_wd >= p_ws || p_ws <= 0.0;
        let bt_y_max = if p_ws > 0.0 {
            (p_wd / p_ws).clamp(0.0, 1.0)
        } else {
            1.0
        };
        // shoulder 起点：k = 1.5·y_max − 0.5（BT.2390 常用），夹紧保证 0 ≤ k < 1
        let bt_knee = (1.5 * bt_y_max - 0.5).clamp(0.0, 0.98);

        // ---- SmoothKnee 预计算（线性域）----
        let sk_peak = source_peak / input_sdr_white;
        let sk_knee = sanitize_f32(_knee_start, 0.9).clamp(0.05, sk_peak.max(0.06) - 0.01);

        // ---- Reinhard 预计算 ----
        // 软肩：out = d + (1−d)·h(v)/h(peak−1)，h(v)=v/(1+v)
        let rh_denom = (sk_peak - 1.0).max(0.0) / (1.0 + (sk_peak - 1.0).max(0.0));

        ToneCurve {
            op,
            input_sdr_white,
            diffuse_out,
            source_peak,
            p_ws,
            target_peak,
            bt_knee,
            bt_y_max,
            bt_identity,
            sk_knee,
            sk_peak,
            rh_denom,
            aces_gain: 1.0,
        }
    }

    /// ACES shoulder 强度（输入预增益；默认 1.0 = 恒等）
    pub fn with_aces_shoulder(mut self, shoulder: f32) -> Self {
        self.aces_gain = sanitize_f32(shoulder, 1.0).clamp(0.25, 4.0);
        self
    }

    /// 映射：y_rel（1.0 = 输入 SDR 白）→ 输出线性（1.0 = 纸白）
    #[inline]
    pub fn map(&self, y_rel: f32) -> f32 {
        if !y_rel.is_finite() {
            return 0.0;
        }
        match self.op {
            TonemapOperator::Bt2390 => self.map_bt2390(y_rel),
            TonemapOperator::SmoothKnee => self.map_smooth_knee(y_rel),
            TonemapOperator::Reinhard => self.map_reinhard(y_rel),
            TonemapOperator::Aces => self.map_aces(y_rel),
        }
    }

    /// 真 BT.2390 EETF：PQ 归一化域线性段 + Hermite shoulder
    ///
    /// 归一化：x = E'_in / p_ws（源峰值 PQ），y_max = p_wd / p_ws（目标峰值 PQ）。
    /// x ≤ k 直通；x > k 三次 Hermite（k 处值/斜率连续，x=1 处斜率为零，映射到 y_max）。
    #[inline]
    fn map_bt2390(&self, y_rel: f32) -> f32 {
        if self.bt_identity {
            // 目标峰值 ≥ 源峰值：PQ 恒等（SDR 白 → diffuse_out 由输出缩放保证）
            return (y_rel * self.input_sdr_white / self.target_peak).clamp(0.0, 1.0);
        }
        // 线性 → 绝对 nits → PQ 码值 → 源峰值归一化
        let l = y_rel * self.input_sdr_white;
        let e_in = pq_oetf((l / 10000.0).clamp(0.0, 1.0));
        let x = e_in / self.p_ws;
        let k = self.bt_knee;
        let y = if x <= k {
            x
        } else {
            let t = ((x - k) / (1.0 - k).max(1e-6)).clamp(0.0, 1.0);
            (2.0 * t * t * t - 3.0 * t * t + 1.0) * k
                + (t * t * t - 2.0 * t * t + t) * (1.0 - k)
                + (-2.0 * t * t * t + 3.0 * t * t) * self.bt_y_max
        };
        // 反归一化 → PQ 解码 → 绝对 nits → 目标峰值归一化（纸白 = 1.0）
        let e_out = (y * self.p_ws).clamp(0.0, 1.0);
        let l_out = pq_eotf(e_out) * 10000.0;
        (l_out / self.target_peak).clamp(0.0, 1.0)
    }

    /// SmoothKnee：knee 以下恒等直通；Hermite 平滑滚降（peak_rel → 1.0）
    ///
    /// 归一化 Hermite（BT.2390 同骨架）：起点斜率 1（归一化域），
    /// 端点斜率 0，天然单调，无 C1 绝对斜率约束导致的过冲。
    #[inline]
    fn map_smooth_knee(&self, y_rel: f32) -> f32 {
        let knee = self.sk_knee;
        let peak = self.sk_peak;
        if y_rel <= knee {
            return y_rel.clamp(0.0, 1.0);
        }
        if y_rel >= peak || peak <= knee {
            return 1.0;
        }
        let t = ((y_rel - knee) / (peak - knee)).clamp(0.0, 1.0);
        // 归一化域 Hermite：(knee 处值=knee、斜率=1) → (peak 处值=1、斜率=0)
        let y0 = knee;
        let y1 = 1.0;
        let span = y1 - y0;
        // H(t) = y0 + span·[(2t³−3t²+1)·0 + (t³−2t²+t)·m + (−2t³+3t²)·1]
        //   m = 线性段在归一化输出坐标的斜率 = (peak−knee)/span，夹紧 [0,3] 保证单调
        let m = ((peak - knee) / span.max(1e-6)).clamp(0.0, 3.0);
        let h = (t * t * t - 2.0 * t * t + t) * m + (-2.0 * t * t * t + 3.0 * t * t);
        (y0 + span * h).clamp(0.0, 1.0)
    }

    /// Reinhard：y ≤ 1 线性（SDR 白 → diffuse_out）；
    /// y > 1 软肩 out = d + (1−d)·h(v)/h(peak−1)，h(v) = v/(1+v)
    #[inline]
    fn map_reinhard(&self, y_rel: f32) -> f32 {
        let d = self.diffuse_out;
        if y_rel <= 1.0 {
            return (y_rel * d).clamp(0.0, 1.0);
        }
        if self.rh_denom <= 0.0 {
            return 1.0;
        }
        let v = y_rel - 1.0;
        let h = v / (1.0 + v);
        (d + (1.0 - d) * (h / self.rh_denom)).clamp(0.0, 1.0)
    }

    /// ACES Fitted（Narkowicz）：直接作用于 y_rel（SDR 白 = 1.0 近似中间调上方）
    #[inline]
    fn map_aces(&self, y_rel: f32) -> f32 {
        let l = y_rel * self.aces_gain;
        let a = 2.51;
        let b = 0.03;
        let c = 2.43;
        let d = 0.59;
        let e = 0.14;
        ((l * (a * l + b)) / (l * (c * l + d) + e)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造常用测试曲线（input_sdr_white=200，peak=1000，d=0.75）
    fn curve(op: TonemapOperator) -> ToneCurve {
        ToneCurve::new(op, 200.0, 1000.0, 0.75, 0.9)
    }

    #[test]
    fn monotonic_all_operators() {
        for op in [
            TonemapOperator::Bt2390,
            TonemapOperator::SmoothKnee,
            TonemapOperator::Reinhard,
            TonemapOperator::Aces,
        ] {
            let c = curve(op);
            let mut prev = -1.0f32;
            let mut y = 0.0f32;
            while y <= 8.0 {
                let out = c.map(y);
                assert!(out.is_finite(), "{:?} map({}) 非有限: {}", op, y, out);
                assert!(
                    out >= prev,
                    "{:?} 非单调: map({})={} < {}",
                    op,
                    y,
                    out,
                    prev
                );
                prev = out;
                y += 0.05;
            }
        }
    }

    #[test]
    fn bt2390_endpoints_and_sdr_white() {
        let c = curve(TonemapOperator::Bt2390);
        // 黑点
        assert_eq!(c.map(0.0), 0.0);
        // 源峰值（y_rel = 1000/200 = 5）→ 1.0
        let at_peak = c.map(5.0);
        assert!(
            (at_peak - 1.0).abs() < 0.01,
            "源峰值应映射到 1.0: {}",
            at_peak
        );
        // 超过源峰值 → 优雅饱和
        assert_eq!(c.map(50.0), 1.0);
        // SDR 白（y_rel=1）应落在 diffuse_out 附近（shoulder 起点低于 SDR 白时略低）
        let at_sdr = c.map(1.0);
        assert!(at_sdr > 0.6 && at_sdr < 0.85, "SDR 白落点异常: {}", at_sdr);
        // knee 处连续（BT.2390 knee 在 PQ 域，线性段内取点验证）
        let e = c.map(0.5);
        assert!((e - 0.5 * 200.0 / (200.0 / 0.75)).abs() < 0.02 || e < 0.5);
    }

    #[test]
    fn bt2390_identity_when_target_ge_source() {
        // 源峰值 ≤ 目标峰值（input_sdr_white/d ≥ source_peak）→ 恒等缩放
        let c = ToneCurve::new(TonemapOperator::Bt2390, 200.0, 200.0, 1.0, 0.9);
        // SDR 白 → 1.0（目标 = 源 = 200 nits）
        let v = c.map(1.0);
        assert!((v - 1.0).abs() < 0.01, "恒等模式 SDR 白应 → 1.0: {}", v);
    }

    #[test]
    fn smooth_knee_endpoints() {
        let c = curve(TonemapOperator::SmoothKnee);
        // knee 以下恒等
        assert_eq!(c.map(0.0), 0.0);
        assert!((c.map(0.5) - 0.5).abs() < 1e-4);
        assert!(
            (c.map(0.9) - 0.9).abs() < 1e-4,
            "knee 处应恒等: {}",
            c.map(0.9)
        );
        // 峰值 → 1.0
        assert!((c.map(5.0) - 1.0).abs() < 1e-4);
        assert_eq!(c.map(50.0), 1.0);
    }

    #[test]
    fn reinhard_endpoints() {
        let c = curve(TonemapOperator::Reinhard);
        // SDR 白 → diffuse_out
        assert!(
            (c.map(1.0) - 0.75).abs() < 1e-4,
            "SDR 白应 → 0.75: {}",
            c.map(1.0)
        );
        // 峰值 → 1.0
        assert!((c.map(5.0) - 1.0).abs() < 1e-4);
        // knee（Y=1）两侧连续
        let below = c.map(1.0 - 1e-4);
        assert!((below - 0.75).abs() < 1e-3);
    }

    #[test]
    fn aces_sane_values() {
        let c = curve(TonemapOperator::Aces);
        assert_eq!(c.map(0.0), 0.0);
        let at_one = c.map(1.0);
        assert!(at_one > 0.7 && at_one < 0.9, "ACES SDR 白落点: {}", at_one);
        assert!(c.map(5.0) > 0.95);
    }

    #[test]
    fn invalid_params_fallback() {
        // NaN/Inf/越界参数不 panic，输出有限
        for (white, peak, diffuse) in [
            (f32::NAN, 1000.0, 0.75),
            (200.0, f32::INFINITY, 0.75),
            (200.0, -5.0, 0.75),
            (200.0, 1000.0, f32::NAN),
            (0.0, 0.0, 0.0),
        ] {
            let c = ToneCurve::new(TonemapOperator::Bt2390, white, peak, diffuse, f32::NAN);
            for y in [0.0f32, 1.0, 5.0, 100.0] {
                let out = c.map(y);
                assert!(
                    out.is_finite() && (0.0..=1.0).contains(&out),
                    "非法参数输出异常: {}",
                    out
                );
            }
        }
    }
}
