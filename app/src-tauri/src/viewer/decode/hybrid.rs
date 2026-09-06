//! 混合解码管线骨架（路线 B；P4+ 条件立项 —— 设计文档 §4.5.1）
//!
//! **当前状态：仅接口骨架，无功能实现。** 触发条件：批量浏览数百上千张
//! JXL 的真实场景出现，或 vendor libjxl 版本进入稳定窗口。在此之前本模块
//! 仅冻结接口设计，防止正式实现时架构走样。
//!
//! # 背景（§4.5.1 结论摘要）
//!
//! libjxl 解码三段中，ANS 熵解码（~65% 耗时）是马尔可夫链式串行位流，
//! GPU 数学上不适合；反量化+IDCT+EPF（~35%，块间/像素独立）适合 GPU。
//! Amdahl 定律：全管线加速上限 ~1.5×——**但系数直出 GPU 纹理可省掉
//! 像素级上传带宽**，这才是本路线的真实收益。
//!
//! # 分工
//!
//! ```text
//! CPU（libjxl 原样）: ANS 熵解码 → 系数流（按 256×256 组导出）
//! GPU（新增 compute）: 反量化 + IDCT（变长块递归展开为 8×8 pass 序列）
//!                      + EPF 去块 + 色域/PQ→scRGB → 直出交换链纹理
//! ```
//!
//! # 需要动的 libjxl 内部（fork 维护负担的来源）
//!
//! - `lib/jxl/dec_group.cc`：组解码完成后导出系数（替代继续 IDCT）
//! - `lib/jxl/dec_cache.h`：系数缓存改为 GPU 友好平铺布局导出
//! - `lib/jxl/decode.cc`：新增公开 API `JxlDecoderSetCoefficientOutput()`
//!
//! # GPU kernel 复杂度来源
//!
//! JXL 的 DCT 是**变长块**（2×2..256×256 递归 DCT8）——非固定 8×8。
//! 策略：递归展开为 8×8 基元 pass 序列 + 中间纹理暂存（每 pass 固定形状，
//! GPU 友好）；EPF 的 sigma 图估计为独立 pass。waifu2x 的 conv compute
//! 模式（R32G32B32A32F 打包 + SRV/UAV 轮转）可直接复用为执行骨架。

/// 熵解码导出的系数组（256×256 组；布局 = GPU IDCT kernel 的输入契约）
#[allow(dead_code)]
pub struct CoefficientGroup {
    /// 组在图像中的位置（256 对齐）
    pub gx: u32,
    pub gy: u32,
    /// 系数（量化后 int；布局待定：CHW 平铺 vs 交错——实现时与 kernel 对拍定）
    pub coeffs: Vec<i32>,
    /// 每通道量化倍率（反量化用）
    pub quant_scale: f32,
}

/// GPU IDCT 后端契约（实现 = waifu2x d3d11.rs 的 compute 会话模式）
#[allow(dead_code)]
pub trait GpuTransformBackend {
    /// 上传一组的系数（structured buffer；FirstElement/NumElements 语义）
    fn upload_group(&mut self, group: &CoefficientGroup) -> Result<(), String>;
    /// 执行反量化+IDCT+EPF dispatch 链 → 直出 scRGB f16 纹理槽位
    fn dispatch_group(&mut self, gx: u32, gy: u32) -> Result<(), String>;
    /// 取整图结果纹理（供交换链呈现；零 CPU 拷贝）
    fn output_texture(&self) -> Result<(), String>;
}

/// 混合解码会话（骨架：字段即数据流设计，实现待立项）
#[allow(dead_code)]
pub struct HybridDecodeSession {
    /// CPU 侧 ANS 熵解码（libjxl fork 提供系数导出）
    // entropy: libjxl_fork::EntropyDecoder,
    /// GPU 侧变换后端
    // gpu: Box<dyn GpuTransformBackend>,
    /// 组完成 → GPU dispatch 的流水线深度（类比录制 staging 池，建议 2）
    pub pipeline_depth: usize,
}

impl HybridDecodeSession {
    /// 骨架占位：恒不可用（未实现）
    pub fn is_available() -> bool {
        false
    }

    /// 骨架占位：真实实现 = open → 逐组（熵解码 CPU / 变换 GPU 交叠）
    pub fn open(_path: &std::path::Path) -> Result<Self, String> {
        Err("混合解码管线未实现（路线 B 骨架，见设计文档 §4.5.1）".to_string())
    }
}

// ===== B2 增量：CPU 黄金重建（对拍锚点） =====
//
// 数学 = libjxl spec 定义（dct_for_test.h / dec_group.cc DequantLane /
// dec_transforms-inl.h LowestFrequenciesFromDC）的纯标量复刻：
//   1. 反量化：bias 校正 × (inv_global_scale/quant) × dequant 矩阵 + CfL
//   2. DC 覆盖：DCT8 块 k=0 ← DC 图值（LowestFrequenciesFromDC 直写）
//   3. IDCT8×8：P = I·C·Iᵀ，I(y,u) = α(u)·√2·cos((y+0.5)·u·π/8)
//
/// DCT8 块类型值（AcStrategyType::DCT = 0）
const ACS_DCT: u8 = 0;
/// CfL tile 的块数（kColorTileDimInBlocks = 64px / 8px）
const COLOR_TILE_IN_BLOCKS: u32 = 8;
const BLOCK_DIM: usize = 8;
const BLOCK_SIZE: usize = 64;

/// spec 1D IDCT 基：I(y,u) = α(u)·√2·cos((y+0.5)·u·π/8)；u=0 时恒为 1
#[inline]
pub fn idct_basis_public(y: usize, u: usize) -> f32 {
    idct_basis(y, u)
}

#[inline]
fn idct_basis(y: usize, u: usize) -> f32 {
    if u == 0 {
        return 1.0;
    }
    let t = (y as f64 + 0.5) * (u as f64) * std::f64::consts::PI / 8.0;
    (std::f64::consts::SQRT_2 * t.cos()) as f32
}

/// spec 1D IDCT（test::IDCT1D<8,8> 标量版）——诊断公开包装
pub fn idct_1d_public(block: &[f32; BLOCK_SIZE], out: &mut [f32; BLOCK_SIZE]) {
    idct_1d(block, out)
}

/// spec 1D IDCT（test::IDCT1D<8,8> 标量版）
///
/// 注意：C++ 实现构造 matrix[y][u] = α(u)√2cos((y+0.5)uπ/8)，但使用时
/// 索引为 matrix[N*u+y]（参数交换）→ 实际 out[u][x] = Σ_y I(u,y)·in[y][x]
fn idct_1d(block: &[f32; BLOCK_SIZE], out: &mut [f32; BLOCK_SIZE]) {
    for x in 0..BLOCK_DIM {
        for u in 0..BLOCK_DIM {
            let mut sum = 0.0f32;
            for y in 0..BLOCK_DIM {
                sum += idct_basis(u, y) * block[y * BLOCK_DIM + x];
            }
            out[u * BLOCK_DIM + x] = sum;
        }
    }
}

fn transpose_in_place(b: &mut [f32; BLOCK_SIZE]) {
    transpose_slice(b)
}

/// 诊断公开包装
pub fn transpose_slice(b: &mut [f32; BLOCK_SIZE]) {
    for y in 0..BLOCK_DIM {
        for x in (y + 1)..BLOCK_DIM {
            b.swap(y * BLOCK_DIM + x, x * BLOCK_DIM + y);
        }
    }
}

/// spec 2D IDCT（IDCTSlow 序列）——诊断公开包装
pub fn idct_2d_8x8_public(block: &[f32; BLOCK_SIZE], out: &mut [f32; BLOCK_SIZE]) {
    idct_2d_8x8(block, out)
}

/// spec 2D IDCT（IDCTSlow 序列：1D → 转置 → 1D → 转置），
/// 与 libjxl ComputeScaledIDCT<8,8> 数值一致（dct_test.cc 对拍保证）
fn idct_2d_8x8(block: &[f32; BLOCK_SIZE], out: &mut [f32; BLOCK_SIZE]) {
    let mut tmp = *block;
    idct_1d(&tmp, out);
    transpose_in_place(out);
    idct_1d(out, &mut tmp);
    *out = tmp;
    transpose_in_place(out);
}

/// AdjustQuantBias 标量版（quantizer-inl.h）：
/// q=0 → 0；|q|=1 → ±biases[c]；否则 q − biases[3]/q
#[inline]
pub fn adjust_quant_bias_public(c: usize, q: i32, biases: &[f32; 4]) -> f32 {
    adjust_quant_bias(c, q, biases)
}

#[inline]
fn adjust_quant_bias(c: usize, q: i32, biases: &[f32; 4]) -> f32 {
    let qf = q as f32;
    if q == 0 {
        return 0.0;
    }
    if q.abs() == 1 {
        return if q < 0 { -biases[c] } else { biases[c] };
    }
    qf - biases[3] / qf
}

/// varblock 覆盖块数 LUT（ac_strategy.h covered_blocks_x/y，按枚举序）
const COVERED_X: [u8; 27] = [
    1, 1, 1, 1, 2, 4, 1, 2, 1, 4, 2, 4, 1, 1, 1, 1, 1, 1, 8, 4, 8, 16, 8, 16, 32, 16, 32,
];
const COVERED_Y: [u8; 27] = [
    1, 1, 1, 1, 2, 4, 2, 1, 4, 1, 4, 2, 1, 1, 1, 1, 1, 1, 8, 8, 4, 16, 16, 8, 32, 32, 16,
];

/// 系数快照（jxl.rs 导出数据的深拷贝，golden/GPU 共用输入契约）
#[derive(Clone)]
pub struct CoeffSnapshot {
    pub width: u32,
    pub height: u32,
    pub xsize_blocks: u32,
    pub ysize_blocks: u32,
    pub group_dim: u32,
    pub num_groups: u32,
    pub xsize_groups: u32,
    /// 3 × num_groups × group_dim²（int16 统一升位 i32）
    pub coeffs: Vec<i32>,
    pub raw_quant_field: Vec<i32>,
    pub ac_strategy: Vec<u8>,
    pub cmap_ytox: Vec<i8>,
    pub cmap_ytob: Vec<i8>,
    pub cmap_tiles_x: u32,
    pub cmap_tiles_y: u32,
    pub dc: Vec<f32>,
    pub dc_xsize: u32,
    pub dc_ysize: u32,
    pub inv_global_scale: f32,
    pub x_dm_multiplier: f32,
    pub b_dm_multiplier: f32,
    pub quant_biases: [f32; 4],
    pub dequant_matrices_dct8: Vec<f32>,
    pub cfl_base_x: f32,
    pub cfl_base_b: f32,
    pub cfl_color_factor: f32,
    /// libjxl 解码期记录的 DCT8 块 IDCT 输出参照层（布局与 coeffs 一致）
    pub idct_ref: Vec<f32>,
    /// B2 诊断参照层（同布局）
    pub dequant_ref: Vec<f32>,
    pub qblock_ref: Vec<i32>,
    /// B2 诊断：C++ 内部对拍 IDCTSlow vs TransformToPixels 输出
    pub idct_slow_max_diff: f64,
    pub idct_slow_samples: u64,
    pub idct_slow_t_max_diff: f64,
    /// B5：XYB→线性 RGB（opsin 逆）参数。逆矩阵已按 255/intensity_target 缩放
    pub opsin_biases: [f32; 4],
    pub opsin_biases_cbrt: [f32; 4],
    /// SIMD 布局 36 float：槽 = (行 j*3+列 i)，每槽广播 4 次，值 = 逆矩阵 [j][i]（行主序）
    pub inverse_opsin_matrix: Vec<f32>,
    /// B6：EPF sigma 图（逻辑尺寸 (xsize_blocks+4)×(ysize_blocks+4)，
    /// 值 = 1/sigma 负数；行距紧凑化 = sigma_xsize；epf_iters = 0 时空向量）
    pub sigma: Vec<f32>,
    pub sigma_xsize: u32,
    pub sigma_ysize: u32,
    /// B6：LoopFilter/Gaborish 帧级参数（与 C 侧 JxlCoefficientExportInfo 一致）
    pub epf_iters: u32,
    pub epf_sharp_lut: [f32; 8],
    pub epf_channel_scale: [f32; 3],
    pub epf_pass0_sigma_scale: f32,
    pub epf_pass2_sigma_scale: f32,
    pub epf_border_sad_mul: f32,
    pub gab: bool,
    pub gab_xweight1: f32,
    pub gab_xweight2: f32,
    pub gab_yweight1: f32,
    pub gab_yweight2: f32,
    pub epf_quant_mul: f32,
    /// Lite 模式（jietu 魔改）：true = 本帧 Lite 导出，dequant_ref/qblock_ref
    /// 为空向量（生产链零消费）；coeffs/idct_ref/sigma/元数据与全量模式一致
    pub lite: bool,
}

/// 黄金重建统计
#[derive(Debug, Clone, Copy, Default)]
pub struct GoldenStats {
    /// 非 DCT8 首块数（这些块 golden 不处理，参照层为 0）
    pub non_dct8_blocks: usize,
}

/// 诊断开关（默认全开 = 与 libjxl 行为一致）
#[derive(Debug, Clone, Copy, Default)]
pub struct GoldenOpts {
    pub disable_cfl: bool,
    pub disable_dc_overwrite: bool,
    /// libjxl 系数块为转置存储（dec_group "JPEG XL is transposed"）：
    /// IDCT 前先转置 8×8 系数块
    pub transpose_coeffs: bool,
    /// IDCT 输出像素块转置后写入（方向实验用）
    pub transpose_output: bool,
}

/// 黄金重建：导出系数 → 反量化 + DC 覆盖 + IDCT8×8 → 块输出层
///
/// 输出布局与 idct_ref 一致：3 × num_groups × group_dim²，decode 顺序
/// （组内块 = 行主序，DCT8 块占 64 float；非 DCT8 块位置填 0 并跳过——
/// 由调用方按 ac_strategy 决定是否用 idct_ref 回退）。
pub fn golden_reconstruct_with_opts(
    s: &CoeffSnapshot,
    opts: &GoldenOpts,
) -> Result<(Vec<f32>, GoldenStats), String> {
    let group_dim = s.group_dim as usize;
    let group_coeffs = group_dim * group_dim;
    let group_dim_blocks = group_dim / BLOCK_DIM;
    let xb = s.xsize_blocks as usize;
    let yb = s.ysize_blocks as usize;
    let xg = s.xsize_groups as usize;
    let tiles_x = s.cmap_tiles_x as usize;
    let dc_x = s.dc_xsize as usize;
    let dc_y = s.dc_ysize as usize;
    if s.dequant_matrices_dct8.len() != 3 * BLOCK_SIZE {
        return Err(format!(
            "dequant_matrices_dct8 尺寸异常: {}",
            s.dequant_matrices_dct8.len()
        ));
    }
    let mut out = vec![0.0f32; 3 * s.num_groups as usize * group_coeffs];
    let mut non_dct8_blocks = 0usize;

    for g in 0..s.num_groups as usize {
        let gx_blocks = (g % xg) * group_dim_blocks;
        let gy_blocks = (g / xg) * group_dim_blocks;
        // 边缘组的实际块数
        let gw = group_dim_blocks.min(xb - gx_blocks.min(xb));
        let gh = group_dim_blocks.min(yb - gy_blocks.min(yb));
        // 组内系数偏移：decode 顺序（行主序，varblock 占 covered×64，与
        // dec_group offset 累积一致；非首块不占偏移）
        let mut offset = 0usize;
        for by in 0..gh {
            for bx in 0..gw {
                let abs_bx = gx_blocks + bx;
                let abs_by = gy_blocks + by;
                let acs_byte = s.ac_strategy[abs_by * xb + abs_bx];
                let kind = (acs_byte >> 1) as usize;
                let is_first = acs_byte & 1 != 0;
                if !is_first {
                    continue;
                }
                let cov_x = COVERED_X[kind] as usize;
                let cov_y = COVERED_Y[kind] as usize;
                let size = cov_x * cov_y * BLOCK_SIZE;
                let is_dct8 = kind == ACS_DCT as usize;
                if !is_dct8 {
                    non_dct8_blocks += 1;
                    offset += size;
                    continue;
                }
                let coeff_base = g * group_coeffs + offset;

                // 1. 反量化（DequantLane 数学）
                let quant = s.raw_quant_field[abs_by * xb + abs_bx];
                let scaled_s = s.inv_global_scale / quant as f32;
                // CfL tile 因子（64px tile）
                let tx = abs_bx / COLOR_TILE_IN_BLOCKS as usize;
                let ty = abs_by / COLOR_TILE_IN_BLOCKS as usize;
                let fx = if opts.disable_cfl {
                    0.0
                } else {
                    s.cfl_base_x + s.cmap_ytox[ty * tiles_x + tx] as f32 / s.cfl_color_factor
                };
                let fb = if opts.disable_cfl {
                    0.0
                } else {
                    s.cfl_base_b + s.cmap_ytob[ty * tiles_x + tx] as f32 / s.cfl_color_factor
                };
                let mut block = [0.0f32; 3 * BLOCK_SIZE];
                for k in 0..BLOCK_SIZE {
                    let q = |c: usize| -> i32 {
                        s.coeffs[c * s.num_groups as usize * group_coeffs + coeff_base + k]
                    };
                    let qy = adjust_quant_bias(1, q(1), &s.quant_biases);
                    let dy = qy * scaled_s * s.dequant_matrices_dct8[1 * BLOCK_SIZE + k];
                    let qx = adjust_quant_bias(0, q(0), &s.quant_biases);
                    let dx =
                        qx * scaled_s * s.x_dm_multiplier * s.dequant_matrices_dct8[k] + fx * dy;
                    let qb = adjust_quant_bias(2, q(2), &s.quant_biases);
                    let db = qb
                        * scaled_s
                        * s.b_dm_multiplier
                        * s.dequant_matrices_dct8[2 * BLOCK_SIZE + k]
                        + fb * dy;
                    block[k] = dx;
                    block[BLOCK_SIZE + k] = dy;
                    block[2 * BLOCK_SIZE + k] = db;
                }

                // 2. DC 覆盖（444：DC 图按块坐标直取；通道序 x,y,b → 0,1,2）
                if abs_by >= dc_y || abs_bx >= dc_x {
                    return Err(format!(
                        "DC 坐标越界: block ({bx},{by}) abs ({abs_bx},{abs_by}) dc {dc_x}x{dc_y}"
                    ));
                }
                if !opts.disable_dc_overwrite {
                    for c in 0..3 {
                        block[c * BLOCK_SIZE] = s.dc[(c * dc_y + abs_by) * dc_x + abs_bx];
                    }
                }

                // 3. IDCT8×8 每通道
                let mut px = [0.0f32; BLOCK_SIZE];
                for c in 0..3 {
                    let mut ch = [0.0f32; BLOCK_SIZE];
                    ch.copy_from_slice(&block[c * BLOCK_SIZE..(c + 1) * BLOCK_SIZE]);
                    if opts.transpose_coeffs {
                        transpose_in_place(&mut ch);
                    }
                    idct_2d_8x8(&ch, &mut px);
                    if opts.transpose_output {
                        transpose_in_place(&mut px);
                    }
                    out[c * s.num_groups as usize * group_coeffs + coeff_base
                        ..c * s.num_groups as usize * group_coeffs + coeff_base + BLOCK_SIZE]
                        .copy_from_slice(&px);
                }
                offset += size;
            }
        }
    }
    Ok((out, GoldenStats { non_dct8_blocks }))
}

/// 默认选项的黄金重建（与 libjxl 行为一致）
pub fn golden_reconstruct(s: &CoeffSnapshot) -> Result<(Vec<f32>, GoldenStats), String> {
    golden_reconstruct_with_opts(
        s,
        &GoldenOpts {
            transpose_coeffs: true,
            ..Default::default()
        },
    )
}

/// B5b：XYB→线性 RGB 黄金参考（dec_xyb-inl.h XybToRgb 逐元素同数学）。
/// 输入 = 块层（XYB 域，3 平面各 n 元素：x,y,b）；输出 = 线性 RGB（sRGB 原色域，
/// 值域 0-255 标度：255 = intensity_target，逆矩阵已含 255/it 缩放）。
/// 注意：此函数在 EPF 之后执行（libjxl 管线序：IDCT → EPF → XYB）。
pub fn xyb_to_linear_ref(s: &CoeffSnapshot, xyb: &[f32]) -> Vec<f32> {
    let n = s.num_groups as usize * s.group_dim as usize * s.group_dim as usize;
    assert_eq!(xyb.len(), 3 * n, "XYB 块层尺寸异常");
    let mut out = vec![0.0f32; 3 * n];
    // SIMD 布局（idx=(j*3+i)*4）→ 3×3 行主序 [j][i]
    let mut m = [[0.0f32; 3]; 3];
    for j in 0..3 {
        for i in 0..3 {
            m[j][i] = s.inverse_opsin_matrix[(j * 3 + i) * 4];
        }
    }
    let biases = s.opsin_biases;
    let cbrt = s.opsin_biases_cbrt;
    for p in 0..n {
        let x = xyb[p];
        let y = xyb[n + p];
        let b = xyb[2 * n + p];
        // 颜色空间：XYB→gamma 域，去偏置后立方（undo simple gamma）
        let gamma_r = y + x - cbrt[0];
        let gamma_g = y - x - cbrt[1];
        let gamma_b = b - cbrt[2];
        let mixed_r = gamma_r * gamma_r * gamma_r + biases[0];
        let mixed_g = gamma_g * gamma_g * gamma_g + biases[1];
        let mixed_b = gamma_b * gamma_b * gamma_b + biases[2];
        // 3×3 逆 opsin 吸收矩阵解混
        out[p] = m[0][0] * mixed_r + m[0][1] * mixed_g + m[0][2] * mixed_b;
        out[n + p] = m[1][0] * mixed_r + m[1][1] * mixed_g + m[1][2] * mixed_b;
        out[2 * n + p] = m[2][0] * mixed_r + m[2][1] * mixed_g + m[2][2] * mixed_b;
    }
    out
}

/// idct_ref（全策略块层）中非 DCT8 块位置清零后的副本——
/// 用于只验证 DCT8 重建正确性的对拍（GPU/黄金共用）
pub fn idct_ref_dct8_only(s: &CoeffSnapshot) -> Vec<f32> {
    let mut r = s.idct_ref.clone();
    let group_dim_blocks = s.group_dim as usize / 8;
    let xb = s.xsize_blocks as usize;
    let xg = s.xsize_groups as usize;
    let ng = s.num_groups as usize;
    let group_coeffs = s.group_dim as usize * s.group_dim as usize;
    for g in 0..s.num_groups as usize {
        let gx = (g % xg) * group_dim_blocks;
        let gy = (g / xg) * group_dim_blocks;
        let gw = group_dim_blocks.min(xb - gx.min(xb));
        let gh = group_dim_blocks.min(s.ysize_blocks as usize - gy.min(s.ysize_blocks as usize));
        let mut offset = 0usize;
        for by in 0..gh {
            for bx in 0..gw {
                let acs = s.ac_strategy[(gy + by) * xb + gx + bx];
                if acs & 1 == 0 {
                    continue;
                }
                let kind = (acs >> 1) as usize;
                let size = COVERED_X[kind] as usize * COVERED_Y[kind] as usize * BLOCK_SIZE;
                if kind != ACS_DCT as usize {
                    for c in 0..3 {
                        let base = c * ng * group_coeffs + g * group_coeffs + offset;
                        r[base..base + size].fill(0.0);
                    }
                }
                offset += size;
            }
        }
    }
    r
}

/// B4 混合模式下 libjxl 跳过 DCT8 的 dump → 一致性对比须将 coeffs 与
/// qblock_ref 的 DCT8 位置**对称清零**后进行（否则 DCT8 位置的 coeffs 非零
/// 会被误判为不一致）。
pub fn coeffs_nondct8_only(s: &CoeffSnapshot) -> Vec<i32> {
    let mut r = s.coeffs.clone();
    zero_dct8_positions(&mut r, s);
    r
}

pub fn qblock_ref_nondct8_only(s: &CoeffSnapshot) -> Vec<i32> {
    let mut r = s.qblock_ref.clone();
    zero_dct8_positions(&mut r, s);
    r
}

fn zero_dct8_positions(v: &mut [i32], s: &CoeffSnapshot) {
    let group_dim_blocks = s.group_dim as usize / 8;
    let xb = s.xsize_blocks as usize;
    let xg = s.xsize_groups as usize;
    let ng = s.num_groups as usize;
    let group_coeffs = s.group_dim as usize * s.group_dim as usize;
    for g in 0..s.num_groups as usize {
        let gx = (g % xg) * group_dim_blocks;
        let gy = (g / xg) * group_dim_blocks;
        let gw = group_dim_blocks.min(xb - gx.min(xb));
        let gh = group_dim_blocks.min(s.ysize_blocks as usize - gy.min(s.ysize_blocks as usize));
        let mut offset = 0usize;
        for by in 0..gh {
            for bx in 0..gw {
                let acs = s.ac_strategy[(gy + by) * xb + gx + bx];
                if acs & 1 == 0 {
                    continue;
                }
                let kind = (acs >> 1) as usize;
                let size = COVERED_X[kind] as usize * COVERED_Y[kind] as usize * BLOCK_SIZE;
                if kind == ACS_DCT as usize {
                    for c in 0..3 {
                        let base = c * ng * group_coeffs + g * group_coeffs + offset;
                        v[base..base + size].fill(0);
                    }
                }
                offset += size;
            }
        }
    }
}

// ===== B6c：Gaborish（帧内去卷积锐化）CPU 黄金参考 =====
//
// 数学 = libjxl render pipeline（render_pipeline/stage_gaborish.cc）精确复刻：
//   本 vendor 版本解码只走 render pipeline（dec_cache.cc PreparePipeline 中
//   `if (loop_filter.gab) builder.AddStage(GetGaborishStage(...))`，位于 EPF
//   stage 之前；dec_group.cc 已无旧 separable5 convolve 路径）→ **单 pass 3×3
//   核**（非 separable 5-tap）。每通道 c∈{x,y,b} 非归一化权重 (1, w1, w2)，
//   归一化分母 div = 1 + 4·(w1+w2)。核形状（对称）：
//     w2  w1  w2
//     w1  w0  w1
//     w2  w1  w2
//   求值序与 libjxl MulAdd 链一致：fma(sum2, w2, fma(sum1, w1, sum0·w0))，
//   其中 sum0 = 中心，sum1 = 左右上下四像素，sum2 = 四对角像素。
//   默认权重（gab_custom=false，libjxl 编码器从不设 true，见 enc_frame.cc
//   LoopFilterFromParams）：w1 = 1.1×0.104699568 = 0.1151695248，
//   w2 = 1.1×0.055680538 = 0.0612485918（三通道同值）。
//   边界 = image_ops.h Mirror（**边缘复制式镜像**：x<0 → -x-1，x≥n → 2n-1-x；
//   即 -1→0、-2→1、n→n-1）——simple/low_memory 两管线填边界均用此函数
//   （simple_render_pipeline.cc row[-1]=row[Mirror(-1)]=row[0]）。
//   作用域 = XYB 域 float 像素（IDCT 之后、EPF 之前）；三通道独立、同形核、
//   各自权重；组间不镜像（邻像素跨组取相邻组同位置槽位，仅图像边界镜像）。

/// libjxl image_ops.h Mirror（边缘复制式镜像；通用步进循环版）
#[inline]
fn mirror_gab(mut x: isize, size: isize) -> usize {
    while x < 0 || x >= size {
        x = if x < 0 { -x - 1 } else { 2 * size - 1 - x };
    }
    x as usize
}

/// 块层布局转换：块主序（每 8×8 块 64 像素按行主序连续存放于该块系数偏移处，
/// 与 golden_reconstruct 写出 / libjxl idct_ref 导出布局一致）→ 组内行主序像素。
/// gab=false 时 GPU 侧由恒等权重 gab pass 完成同一转换（B8 端到端对拍锚定：
/// 块主序写 / 行主序读的错位曾导致整帧块级打乱）。
fn block_major_to_row_major(s: &CoeffSnapshot, xyb: &[f32]) -> Vec<f32> {
    let gd = s.group_dim as usize;
    let gdc = gd * gd;
    let n = s.num_groups as usize * gdc;
    let mut out = vec![0f32; 3 * n];
    for c in 0..3 {
        for g in 0..s.num_groups as usize {
            for iy in 0..gd {
                for ix in 0..gd {
                    let blk = (iy / 8) * (gd / 8) + ix / 8;
                    let src = g * gdc + blk * 64 + (iy % 8) * 8 + (ix % 8);
                    out[c * n + g * gdc + iy * gd + ix] = xyb[c * n + src];
                }
            }
        }
    }
    out
}

/// Gaborish CPU 黄金参考（stage_gaborish.cc 逐像素同数学）。
/// 输入/输出 = 全帧块层（[c][num_groups][group_dim²]，与 full_out 同布局，
/// XYB 域）。只在真实图像区（px<width 且 py<height）执行滤波；图像边界按
/// libjxl Mirror 镜像取邻像素（可跨组）；填充槽位原样透传。
/// gab=false → 跳过滤波，但仍做块主序→行主序布局转换（与 GPU 恒等权重
/// gab pass 对齐，见 [`block_major_to_row_major`]）。
/// 注意：b 通道权重未随快照导出（JxlCoefficientExportInfo 无 gab_b 字段），
/// libjxl 编码器恒用默认权重（三通道同值），故 b 通道复用 y 权重对。
pub fn gaborish_ref(s: &CoeffSnapshot, xyb: &[f32]) -> Result<Vec<f32>, String> {
    if !s.gab {
        return Ok(block_major_to_row_major(s, xyb));
    }
    let gd = s.group_dim as usize;
    let gdc = gd * gd;
    let n = s.num_groups as usize * gdc;
    if xyb.len() != 3 * n {
        return Err(format!("XYB 块层尺寸异常: {} ≠ 3×{}", xyb.len(), 3 * n));
    }
    let xg = s.xsize_groups as usize;
    let xsize = s.width as isize;
    let ysize = s.height as isize;
    // 每通道归一化权重 [w0, w1, w2]（stage_gaborish.cc Normalize 同式）
    let mut w = [[0.0f32; 3]; 3];
    for (c, (w1, w2)) in [
        (s.gab_xweight1, s.gab_xweight2),
        (s.gab_yweight1, s.gab_yweight2),
        (s.gab_yweight1, s.gab_yweight2), // b：未导出，编码器恒用默认（与 x/y 同值）
    ]
    .iter()
    .enumerate()
    {
        let div = 1.0 + 4.0 * (w1 + w2);
        w[c] = [1.0 / div, w1 / div, w2 / div];
    }
    // 像素→块层槽位采样 helper（全局坐标 → (组, 组内偏移)；边界镜像，跨组寻址）。
    // 块层为块主序：每 8×8 块的 64 像素按行主序连续存放于该块的系数偏移处
    // （与 golden_reconstruct 写出 / libjxl idct_ref 导出布局一致）——B8 端到端
    // 对拍发现按行主序误读导致整帧块级打乱，此处按块主序解码。
    let sample = |px: isize, py: isize, c: usize| -> f32 {
        let mx = mirror_gab(px, xsize);
        let my = mirror_gab(py, ysize);
        let g = (my / gd) * xg + (mx / gd);
        let (lx, ly) = ((mx % gd) as usize, (my % gd) as usize);
        let idx = ((ly / 8) * (gd / 8) + lx / 8) * 64 + (ly % 8) * 8 + lx % 8;
        xyb[c * n + g * gdc + idx]
    };
    let mut out = xyb.to_vec(); // 填充槽位（px≥width 或 py≥height）原样透传
    for py in 0..ysize {
        for px in 0..xsize {
            for c in 0..3 {
                let (w0, w1, w2) = (w[c][0], w[c][1], w[c][2]);
                let sum0 = sample(px, py, c);
                let sum1 = sample(px - 1, py, c)
                    + sample(px + 1, py, c)
                    + sample(px, py - 1, c)
                    + sample(px, py + 1, c);
                let sum2 = sample(px - 1, py - 1, c)
                    + sample(px + 1, py - 1, c)
                    + sample(px - 1, py + 1, c)
                    + sample(px + 1, py + 1, c);
                // libjxl：MulAdd(sum2, w2, MulAdd(sum1, w1, Mul(sum0, w0)))
                let v = sum2.mul_add(w2, sum1.mul_add(w1, sum0 * w0));
                let g = (py as usize / gd) * xg + (px as usize / gd);
                let idx = (py as usize % gd) * gd + (px as usize % gd);
                out[c * n + g * gdc + idx] = v;
            }
        }
    }
    Ok(out)
}

// ===== B6d：EPF1（edge-preserving filter 第一遍）CPU 黄金参考 =====
//
// 数学 = libjxl render pipeline（render_pipeline/stage_epf.cc 197-379 EpfStage::One）
// 逐像素精确复刻（dec_cache.cc PreparePipeline：epf_iters>=1 加 EPF1，位于
// Gaborish 之后；epf_iters=1 → 仅 EPF1 单 pass）：
//   - 5×5 去角支撑（13 像素），4 邻居（上/左/右/下）各一个 plus-SAD：
//     SAD_d = Σ_{o∈plus} Σ_c channel_scale[c]·|P(c,p+o) − P(c,p+d+o)|
//     plus = {(0,0),(-1,0),(0,-1),(1,0),(0,1)}（源码 277-336 手写展开的
//     sad0..sad3 与此完全等价，各 5 个像素对；P = Mirror 边界采样）。
//   - inv_sigma = row_sigma[bx] · sad_mul —— **sad_mul 只乘这一次**（源码
//     270-271：先取 vsm = sad_mul[ix] 再乘 row_sigma；不是乘进 SAD）。
//     sad_mul（源码 240-255 8 元素模式）：iy==0||7 → 整行 bsm；否则
//     ix==0||7 → bsm，中间 sm = 1.65（EPF1 固定，非 pass0_sigma_scale）。
//     bsm = sm · epf_border_sad_mul。
//   - weight_d = max(0, 1 + SAD_d · inv_sigma)（Weight：ZeroIfNegative，
//     epf_pass1_zeroflush 参数在本 vendor 版未使用；sigma 存负值 → SAD 越
//     大权重越小）。
//   - 输出 = (center + Σ w_d·p_d) · (1/(1+Σ w_d))（JXL_HIGH_PRECISION 默认
//     开 → 精确除法求倒数再乘）。
//   - 跳过：row_sigma[bx] < kMinSigma → 该块三通道直拷（源码 262-268）；
//     epf_iters=0 或 sigma 空 → 无 EPF stage，整帧直拷。
//   - sigma 索引 = (py/8+2)·sigma_xsize + (px/8+2)：dec_cache.h kSigmaPadding=2，
//     sigma 图逻辑尺寸 (xsize_blocks+4)×(ysize_blocks+4)，导出整图含 padding
//     环（dec_frame.cc 973-980 逐行紧凑拷贝），帧边镜像已由 ComputeSigma 填好。
//   - 边界 = image_ops.h Mirror（同 Gaborish：-1→0、-2→1、n→n-1）。

/// epf.h kMinSigma（row_sigma < 此值 → 该块不滤波；负值域阈值）
pub const K_MIN_SIGMA: f32 = -3.90524291751269967465540850526868;

/// EPF1 的 13 个采样偏移（dy,dx；5×5 去角，索引即 OFFS 顺序）
const EPF1_OFFS: [(isize, isize); 13] = [
    (0, 0),   // 0  中心 p22
    (-1, 0),  // 1  上 p21
    (0, -1),  // 2  左 p12
    (0, 1),   // 3  右 p32
    (1, 0),   // 4  下 p23
    (-2, 0),  // 5  上² p20
    (0, -2),  // 6  左² p02
    (0, 2),   // 7  右² p42
    (2, 0),   // 8  下² p24
    (-1, -1), // 9  左上 p11
    (-1, 1),  // 10 右上 p31
    (1, -1),  // 11 左下 p13
    (1, 1),   // 12 右下 p33
];

/// 4 邻居 plus-SAD 的 5 像素对（OFFS 索引；序 = 源码 277-336 展开式，
/// |a-b| 对称故对内顺序无关）：
/// sad0=上: |p20-p21|+|p11-p12|+|p22-p21|+|p31-p32|+|p23-p22|
/// sad1=左: |p21-p11|+|p12-p02|+|p22-p12|+|p32-p22|+|p23-p13|
/// sad2=右: |p22-p32|+|p21-p31|+|p12-p22|+|p32-p42|+|p23-p33|
/// sad3=下: |p22-p23|+|p21-p22|+|p12-p13|+|p32-p33|+|p23-p24|
const EPF1_SAD_PAIRS: [[(usize, usize); 5]; 4] = [
    [(5, 1), (9, 2), (0, 1), (10, 3), (4, 0)],  // 上
    [(1, 9), (2, 6), (0, 2), (3, 0), (4, 11)],  // 左
    [(1, 10), (2, 0), (0, 3), (3, 7), (4, 12)], // 右
    [(1, 0), (2, 11), (0, 4), (3, 12), (4, 8)], // 下
];

/// EPF1 CPU 黄金参考（stage_epf.cc EpfStage::One 逐像素同数学）。
/// 输入 = Gaborish 输出块层（[c][num_groups][group_dim²]，XYB 域）；输出同布局。
/// 只在真实图像区（px<width 且 py<height）滤波；边界按 libjxl Mirror 镜像
/// （可跨组）；填充槽位原样透传。epf_iters=0 或 sigma 空 → 直拷返回。
pub fn epf1_ref(s: &CoeffSnapshot, xyb: &[f32]) -> Result<Vec<f32>, String> {
    if s.epf_iters == 0 || s.sigma.is_empty() {
        return Ok(xyb.to_vec());
    }
    let gd = s.group_dim as usize;
    let gdc = gd * gd;
    let n = s.num_groups as usize * gdc;
    if xyb.len() != 3 * n {
        return Err(format!("XYB 块层尺寸异常: {} ≠ 3×{}", xyb.len(), 3 * n));
    }
    let sbx = s.sigma_xsize as usize;
    let sby = s.sigma_ysize as usize;
    if s.sigma.len() < sbx * sby {
        return Err(format!(
            "sigma 图尺寸异常: {} < {}×{}",
            s.sigma.len(),
            sbx,
            sby
        ));
    }
    let xg = s.xsize_groups as usize;
    let xsize = s.width as isize;
    let ysize = s.height as isize;
    let cs = s.epf_channel_scale;
    let sm = 1.65f32;
    let bsm = sm * s.epf_border_sad_mul;
    let sigma = &s.sigma;
    let mut out = xyb.to_vec();
    for py in 0..ysize {
        for px in 0..xsize {
            // 13 个采样槽位（通道无关，一次算好；槽位 0 = 本像素输出槽位）
            let mut slots = [0usize; 13];
            for (i, &(dy, dx)) in EPF1_OFFS.iter().enumerate() {
                let mx = mirror_gab(px + dx, xsize);
                let my = mirror_gab(py + dy, ysize);
                slots[i] = ((my / gd) * xg + (mx / gd)) * gdc + (my % gd) * gd + (mx % gd);
            }
            // sigma 采样：块坐标 (px/8, py/8) + kSigmaPadding=2
            let sig = sigma[((py as usize / 8) + 2) * sbx + ((px as usize / 8) + 2)];
            if sig < K_MIN_SIGMA {
                continue; // 低 sigma 块不滤波（out 已是输入直拷）
            }
            let ix = px as usize % 8;
            let iy = py as usize % 8;
            let sad_mul = if iy == 0 || iy == 7 || ix == 0 || ix == 7 {
                bsm
            } else {
                sm
            };
            let inv_sigma = sig * sad_mul;
            for c in 0..3 {
                let v = |i: usize| xyb[c * n + slots[i]];
                let mut acc = v(0);
                let mut wsum = 1.0f32;
                for (d, pairs) in EPF1_SAD_PAIRS.iter().enumerate() {
                    let mut sad = 0.0f32;
                    for &(a, b) in pairs {
                        sad += (v(a) - v(b)).abs();
                    }
                    sad *= cs[c];
                    let w = (1.0 + sad * inv_sigma).max(0.0);
                    wsum += w;
                    acc += w * v(d + 1); // 邻居像素 = OFFS[d+1]
                }
                out[c * n + slots[0]] = acc * (1.0 / wsum);
            }
        }
    }
    Ok(out)
}

// ===== B7：PQ16 BT.2020（u16）输出 CPU 黄金参考 =====
//
// 语义 = Native 解码出口约定（jxl.rs AnimationDecoder::next_frame 消费侧，
// jxl.rs:749-772）:u16 值 v ⇔ BT.2020 原色、PQ 编码，nits = pq_decode(v/65535)×10000。
//
// # 变换链（libjxl 源码逐行锤定，GPU pass O 同数学）
//
//   1. 原色域：**导出的逆 opsin 矩阵已含 709→2020 原色烘焙**（对 BT.2020 PQ 文件）。
//      dec_xyb.cc:181-250 SetColorEncoding：c_desired 原色 ≠ kSRGB 且 xyb_encoded →
//      `inverse_matrix = srgb_to_original × orig_inverse_matrix`（212-223 行，
//      srgb_to_original = 2020 的 XYZ(D50)⁻¹ × sRGB 的 XYZ(D50)，Bradford 往返 =
//      标准线性原色矩阵）；随后 InitSIMDInverseMatrix ×255/it（237-242 行）。
//      系数导出（dec_frame.cc:960-968）拷贝的正是这个烘焙矩阵。
//   2. nits 映射：**nits = linear × intensity_target（1.0 = it nits，非 0-255 标度）**。
//      证据链：InitSIMDInverseMatrix（opsin_params.cc:35-45）乘 255/it，其中 255 =
//      SDR 白 nits 常数——未缩放矩阵输出是"绝对亮度" nits/255（dec_xyb.cc:234-236
//      注释 "scale back ... where 1.0 corresponds to the original intensity target"）；
//      stage_from_linear.cc:56-66 OpPq 把 XYB 输出**无 /255 直喂**
//      TF_PQ(it).EncodedFromDisplay（transfer_functions-inl.h:135-212，内部 ×it/10000
//      归一到 10000 nits 再 PQ 有理多项式编码）→ 输入必为 nits/it。
//      终端佐证：dec_external_image.cc:49-72 FloatToU32 = NearestInt(clamp(E,0,1)×65535)。
//   3. intensity_target：metadata.m.IntensityTarget()（dec_xyb.cc:146）=
//      tone_mapping.intensity_target（image_metadata.h:263-266；码流 F16 字段默认
//      255 = kDefaultIntensityTarget，base/common.h:104、image_metadata.cc:392-396）。
//      CoeffSnapshot 未导出该字段 → 从导出矩阵反解：导出矩阵 = 母矩阵 × k，
//      k = 255/it（InitSIMDInverseMatrix 唯一缩放）→ it = 255/k。
//      母矩阵两个候选（运行时比例拟合自动判定，见 [`pq16_out_params`]）：
//      A. sRGB 域 = kDefaultInverseOpsinAbsorbanceMatrix（cms/opsin_params.h:44-47，
//         码流默认值 image_metadata.cc:363-371）
//      B. 2020 烘焙域 = (709→2020) × A（dec_xyb.cc:212-239；录制样本 BT.2020 PQ
//         命中此情形）
//
// # PQ 编码（ST 2084，与 crate::color::transfer.rs 同常数，HLSL pow 同式）
const PQ_M1: f32 = 0.1593017578125; // 2610/16384
const PQ_M2: f32 = 78.84375; // 2523/4096 × 128
const PQ_C1: f32 = 0.8359375; // 3424/4096
const PQ_C2: f32 = 18.8515625; // 2413/4096 × 32
const PQ_C3: f32 = 18.6875; // 2392/4096 × 32

/// libjxl kDefaultInverseOpsinAbsorbanceMatrix（cms/opsin_params.h:44-47，行主序）
pub const K_DEFAULT_INV_OPSIN: [[f32; 3]; 3] = [
    [11.031566901960783, -9.866943921568629, -0.16462299647058826],
    [-3.254147380392157, 4.418770392156863, -0.16462299647058826],
    [-3.6588512862745097, 2.7129230470588235, 1.9459282392156863],
];

/// PQ OETF（L = nits/10000 ∈ [0,1] → 编码值）+ u16 量化（round，≥65535 饱和）。
/// 域外裁剪与 Native FloatToU32 的 clamp(E,0,1)（dec_external_image.cc:65）等价。
#[inline]
fn pq_encode_u16(l: f32) -> u16 {
    let l = l.clamp(0.0, 1.0);
    let lm = l.powf(PQ_M1);
    let np = (PQ_C1 + PQ_C2 * lm) / (1.0 + PQ_C3 * lm);
    let e = np.powf(PQ_M2);
    (e * 65535.0 + 0.5).min(65535.0) as u16
}

/// 从导出的 SIMD 逆 opsin 矩阵反解 (intensity_target nits, pass O 色域矩阵)。
///
/// 导出矩阵 = 母矩阵 × k（k = 255/it，InitSIMDInverseMatrix 唯一缩放），对两个
/// 候选母矩阵做最小二乘比例拟合，取残差小者：
/// - A（sRGB 域）：母矩阵 = kDefaultInverseOpsinAbsorbanceMatrix → pass D 输出
///   线性 sRGB 域 → pass O 需乘 709→2020（crate::color::bt2020_to_bt709 数值求逆）
/// - B（2020 烘焙域）：母矩阵 = (709→2020) × 默认矩阵（dec_xyb.cc:212-239）→
///   pass D 输出已是 BT.2020 线性 → pass O 乘恒等
///
/// 返回 (it_nits, 色域矩阵)。两假设残差均过大 → 文件用了自定义 opsin 矩阵，报错。
pub fn pq16_out_params(s: &CoeffSnapshot) -> Result<(f32, crate::color::Mat3), String> {
    // SIMD 布局（idx=(j*3+i)*4 广播 4 次）→ 3×3 行主序
    let mut e = [[0.0f32; 3]; 3];
    for j in 0..3 {
        for i in 0..3 {
            e[j][i] = s.inverse_opsin_matrix[(j * 3 + i) * 4];
        }
    }
    let da = crate::color::Mat3 { m: K_DEFAULT_INV_OPSIN };
    // 709→2020（BT.2020_to_BT.709 的数值逆，两矩阵同构于 gamut_matrix 构造）
    let m2020 = crate::color::bt2020_to_bt709().inverse();
    let db = m2020.mul(&da);
    // 最小二乘比例拟合：k = ⟨E,P⟩/⟨P,P⟩；相对残差 = max|E−kP|/max|kP|
    fn fit(e: &[[f32; 3]; 3], p: &crate::color::Mat3) -> (f32, f32) {
        let mut sep = 0.0f32;
        let mut spp = 0.0f32;
        for j in 0..3 {
            for i in 0..3 {
                sep += e[j][i] * p.m[j][i];
                spp += p.m[j][i] * p.m[j][i];
            }
        }
        let k = sep / spp;
        let mut max_res = 0.0f32;
        let mut max_p = 0.0f32;
        for j in 0..3 {
            for i in 0..3 {
                max_res = max_res.max((e[j][i] - k * p.m[j][i]).abs());
                max_p = max_p.max((k * p.m[j][i]).abs());
            }
        }
        (k, if max_p > 0.0 { max_res / max_p } else { f32::INFINITY })
    }
    let (ka, ra) = fit(&e, &da);
    let (kb, rb) = fit(&e, &db);
    let srgb_domain = ra <= rb;
    let (k, r) = if srgb_domain { (ka, ra) } else { (kb, rb) };
    if r > 1e-3 {
        return Err(format!(
            "导出逆 opsin 矩阵与两假设均不符（残差 A[sRGB域]={ra:.3e} B[2020烘焙]={rb:.3e}）：文件可能使用自定义 opsin 矩阵"
        ));
    }
    let it = 255.0 / k;
    // 每帧调用（播放热路径）：剖析开关下打印，默认静默
    if super::prof_enabled() {
        println!(
            "[prof] pq16_out_params: 反解 intensity_target = {:.2} nits（残差 A[sRGB域]={:.3e} B[2020烘焙]={:.3e}，选定 {}；导出矩阵[0][0]={:.10}）",
            it,
            ra,
            rb,
            if srgb_domain {
                "A: sRGB 域 → pass O 乘 709→2020"
            } else {
                "B: 709→2020 已烘焙 → pass O 恒等"
            },
            s.inverse_opsin_matrix[0]
        );
    }
    let gamut = if srgb_domain { m2020 } else { crate::color::Mat3::IDENTITY };
    Ok((it, gamut))
}

/// B7：PQ16 BT.2020（u16）输出 CPU 黄金参考。
/// 输入 = 线性 RGB 块层（[c][num_groups][group_dim²] 三平面，pass D / xyb_to_linear_ref
/// 输出；1.0 = intensity_target nits）；输出 = 同布局 u16 三平面（PQ BT.2020，
/// v ⇔ nits = pq_decode(v/65535)×10000，Native 消费侧语义）。仅处理有效像素
/// （px<xsize 且 py<ysize），无效槽位填 0（与 GPU pass O 一致）。
pub fn pq16_out_ref(s: &CoeffSnapshot, linear: &[f32]) -> Result<Vec<u16>, String> {
    let gd = s.group_dim as usize;
    let gdc = gd * gd;
    let n = s.num_groups as usize * gdc;
    if linear.len() != 3 * n {
        return Err(format!("linear 层尺寸异常: {} ≠ 3×{}", linear.len(), 3 * n));
    }
    let (it, gamut) = pq16_out_params(s)?;
    let mut out = vec![0u16; 3 * n]; // 无效槽位保持 0
    let k = it / 10000.0; // nits/it → PQ 域（1.0 = 10000 nits）
    let xg = s.xsize_groups as usize;
    for py in 0..s.height as usize {
        for px in 0..s.width as usize {
            let g = (py / gd) * xg + px / gd;
            let idx = (py % gd) * gd + (px % gd);
            let p = g * gdc + idx;
            let (r1, g1, b1) = gamut.apply(linear[p], linear[n + p], linear[2 * n + p]);
            out[p] = pq_encode_u16(r1 * k);
            out[n + p] = pq_encode_u16(g1 * k);
            out[2 * n + p] = pq_encode_u16(b1 * k);
        }
    }
    Ok(out)
}

/// 对拍统计：golden vs libjxl idct_ref（以 max|ref| 为 PSNR 峰值）
#[derive(Debug, Clone, Copy)]
pub struct CompareResult {
    pub psnr_db: f64,
    pub max_abs_diff: f32,
    pub mse: f64,
    pub peak: f32,
    pub samples: usize,
}

pub fn compare_against_ref(golden: &[f32], idct_ref: &[f32]) -> Result<CompareResult, String> {
    if golden.len() != idct_ref.len() || golden.is_empty() {
        return Err(format!(
            "长度不匹配: golden {} vs ref {}",
            golden.len(),
            idct_ref.len()
        ));
    }
    let mut mse = 0.0f64;
    let mut max_diff = 0.0f32;
    let mut peak = 0.0f32;
    for (a, b) in golden.iter().zip(idct_ref.iter()) {
        // 非 DCT8 块两侧均为 0，自然不引入误差
        let d = (a - b) as f64;
        mse += d * d;
        max_diff = max_diff.max(a - b).max(b - a);
        peak = peak.max(b.abs());
    }
    let n = golden.len();
    mse /= n as f64;
    let psnr = if mse <= 1e-20 {
        f64::INFINITY
    } else {
        10.0 * ((peak as f64) * (peak as f64) / mse).log10()
    };
    Ok(CompareResult {
        psnr_db: psnr,
        max_abs_diff: max_diff,
        mse,
        peak,
        samples: n,
    })
}
