//! HDRTVNet（Ensemble_AGCM_LE）SDR→HDRTV 推理——P9 AI 智能档
//!
//! 论文：A New Journey from SDRTV to HDRTV (ICCV 2021, Chao Dong 组)
//!       HDRTVNet-plus 扩展版的 Ensemble_AGCM_LE 合并单网（AGCM+LE 一步直出）
//! 权重：models/itm/Ensemble_AGCM_LE.pth（官方预训练，2.3MB，264 变量）
//!
//! 结构（对照官方 Ensemble_AGCM_LE_arch.py / Condition_arch.py / arch_util.py 逐行核对）：
//! - AGCM：Color_Condition 条件分支（5×(1×1conv→AvgPool3/2→LeakyReLU0.2→InstanceNorm)
//!   + Dropout + 1×1conv + GAP → 6 维条件向量）→ 6 个 Linear 生成 scale/shift
//!   → 3 段 1×1 conv 残差调制（out = out·(1+s) + sh）
//! - LE：HDRUNet3T1 U-Net（3 级 stride-conv 下采样 + 8×ResBlock_with_SFT + 3 级
//!   Conv+PixelShuffle(2) 上采样 + 长短跳连 + 末端残差 out = input + gain）
//!
//! 口径（官方 test.py 确认）：输入 = SDR sRGB [0,1] RGB CHW；输出 = **PQ 编码 BT.2020** [0,1]
//! （GT 即 HDR10 帧的 PQ 值），外部接 inverse-PQ → nits。
//!
//! v1 为 CPU 推理（正确性优先）；nf=32 轻量网，1080p 全图 CPU 数秒，
//! GPU（ExecLayer 化）留作后续加速。

use std::collections::HashMap;
use std::path::Path;

use super::cpu::Tensor;
use super::pth::{PthFile, PthTensor};

const IN_EPS: f32 = 1e-5; // PyTorch InstanceNorm2d 默认 eps

/// 分块边长（8 的倍数；512² 块 CPU 数秒级）
const TILE: usize = 512;
/// 分块重叠（抑制块边界接缝；覆盖网络感受野足够）
const OVERLAP: usize = 32;

/// AI 权重路径（models\itm\Ensemble_AGCM_LE.pth，官方预训练）
pub fn default_model_path() -> std::path::PathBuf {
    super::pipeline::default_model_root()
        .join("itm")
        .join("Ensemble_AGCM_LE.pth")
}

// ============================ 权重容器 ============================

/// (weight, bias) 对
struct CB {
    w: Vec<f32>,
    b: Vec<f32>,
    oc: usize,
    ic: usize,
    k: usize,
}

impl CB {
    fn load(
        map: &HashMap<String, PthTensor>,
        pth: &mut PthFile,
        prefix: &str,
    ) -> Result<Self, String> {
        let wt = map
            .get(&format!("{}.weight", prefix))
            .ok_or_else(|| format!("权重缺失: {}.weight", prefix))?;
        let w = pth.get_f32(&format!("{}.weight", prefix))?;
        let b = pth.get_f32(&format!("{}.bias", prefix))?;
        if wt.shape.len() != 4 {
            return Err(format!("{} 权重非 4 维: {:?}", prefix, wt.shape));
        }
        Ok(CB {
            w,
            b,
            oc: wt.shape[0],
            ic: wt.shape[1],
            k: wt.shape[2],
        })
    }
}

/// (gamma, beta) 对
struct GB {
    g: Vec<f32>,
    b: Vec<f32>,
}

/// SFTLayer 权重（scale/shift 各两层 1×1）
struct SftW {
    s0: CB,
    s1: CB,
    h0: CB,
    h1: CB,
}

impl SftW {
    fn load(
        load_cb: &mut dyn FnMut(&str) -> Result<CB, String>,
        prefix: &str,
    ) -> Result<Self, String> {
        Ok(SftW {
            s0: load_cb(&format!("{}.SFT_scale_conv0", prefix))?,
            s1: load_cb(&format!("{}.SFT_scale_conv1", prefix))?,
            h0: load_cb(&format!("{}.SFT_shift_conv0", prefix))?,
            h1: load_cb(&format!("{}.SFT_shift_conv1", prefix))?,
        })
    }
}

/// ResBlock_with_SFT
struct ResSft {
    sft1: SftW,
    conv1: CB,
    sft2: SftW,
    conv2: CB,
}

impl ResSft {
    fn load(
        load_cb: &mut dyn FnMut(&str) -> Result<CB, String>,
        prefix: &str,
    ) -> Result<Self, String> {
        Ok(ResSft {
            sft1: SftW::load(load_cb, &format!("{}.sft1", prefix))?,
            conv1: load_cb(&format!("{}.conv1", prefix))?,
            sft2: SftW::load(load_cb, &format!("{}.sft2", prefix))?,
            conv2: load_cb(&format!("{}.conv2", prefix))?,
        })
    }
}

/// 完整模型
pub struct HdrItm {
    // AGCM classifier：5 conv + 4 InstanceNorm（γ,β）+ 末端 128→cond_c 1×1 conv
    cc_convs: Vec<CB>,
    cc_ins: Vec<GB>,
    cc_conv_out: CB,
    // AGCM 6 Linear + 3 调制主干 1×1 conv
    lin_scale: [CB; 3], // first/HR/last
    lin_shift: [CB; 3],
    agcm_convs: [CB; 3], // conv_first/HRconv/conv_last
    // LE
    le_conv_first: CB,
    le_sft1: SftW,
    le_hr_conv1: CB,
    le_down: Vec<CB>,
    le_cond_first: Vec<CB>,
    le_cond_nets: Vec<Vec<CB>>, // CondNet1-4，各 3 层
    le_trunks: Vec<ResSft>,     // 8 块（trunk1..5 = 1+1+4+1+1）
    le_up: Vec<CB>,             // up_conv1-3（nf→nf*4，PixelShuffle(2)）
    le_sft2: SftW,
    le_hr_conv2: CB,
    le_conv_last: CB,
}

/// color_block 展平 Sequential 的 conv 索引 / InstanceNorm 索引
const CC_CONV_IDX: [usize; 5] = [0, 4, 8, 12, 16];
const CC_IN_IDX: [usize; 4] = [3, 7, 11, 15];
/// trunk 块数（recon_trunk1..5）
const TRUNK_BLOCKS: [usize; 5] = [1, 1, 4, 1, 1];

impl HdrItm {
    pub fn load(pth_path: &Path) -> Result<Self, String> {
        let mut pth = PthFile::open(pth_path)?;
        let names: Vec<String> = pth.names().into_iter().cloned().collect();
        let map: HashMap<String, PthTensor> = names
            .into_iter()
            .filter_map(|n| pth.tensor(&n).cloned().map(|t| (n, t)))
            .collect();
        // AGCM classifier
        let cc_convs: Vec<CB> = CC_CONV_IDX
            .iter()
            .map(|i| CB::load(&map, &mut pth, &format!("AGCM.classifier.model.{}", i)))
            .collect::<Result<_, _>>()?;
        let cc_ins: Vec<GB> = CC_IN_IDX
            .iter()
            .map(|i| {
                Ok(GB {
                    g: pth.get_f32(&format!("AGCM.classifier.model.{}.weight", i))?,
                    b: pth.get_f32(&format!("AGCM.classifier.model.{}.bias", i))?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        // AGCM linears（Linear 在 BasicSR 里无 4 维 shape——pth.rs 也能存；CB::load 要求 4 维，
        // Linear weight 是 2 维 [out,in] → 手工加载）
        let mut lin2 = |prefix: &str| -> Result<CB, String> {
            let t = map
                .get(&format!("{}.weight", prefix))
                .ok_or_else(|| format!("权重缺失: {}.weight", prefix))?;
            if t.shape.len() != 2 {
                return Err(format!("{} 非 Linear（{:?}）", prefix, t.shape));
            }
            Ok(CB {
                w: pth.get_f32(&format!("{}.weight", prefix))?,
                b: pth.get_f32(&format!("{}.bias", prefix))?,
                oc: t.shape[0],
                ic: t.shape[1],
                k: 1,
            })
        };
        let lin_scale = [
            lin2("AGCM.cond_scale_first")?,
            lin2("AGCM.cond_scale_HR")?,
            lin2("AGCM.cond_scale_last")?,
        ];
        let lin_shift = [
            lin2("AGCM.cond_shift_first")?,
            lin2("AGCM.cond_shift_HR")?,
            lin2("AGCM.cond_shift_last")?,
        ];
        let mut cb = |prefix: &str| -> Result<CB, String> { CB::load(&map, &mut pth, prefix) };
        let agcm_convs = [
            cb("AGCM.conv_first")?,
            cb("AGCM.HRconv")?,
            cb("AGCM.conv_last")?,
        ];

        // LE
        let le_cond_first = vec![
            cb("LE.cond_first.0")?,
            cb("LE.cond_first.2")?,
            cb("LE.cond_first.4")?,
        ];
        let le_cond_nets = (1..=4)
            .map(|n| {
                Ok(vec![
                    cb(&format!("LE.CondNet{}.0", n))?,
                    cb(&format!("LE.CondNet{}.2", n))?,
                    cb(&format!("LE.CondNet{}.4", n))?,
                ])
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut le_trunks = Vec::new();
        for (tn, nb) in TRUNK_BLOCKS.iter().enumerate() {
            for b in 0..*nb {
                le_trunks.push(ResSft::load(
                    &mut cb,
                    &format!("LE.recon_trunk{}.{}", tn + 1, b),
                )?);
            }
        }
        let le_up = vec![
            cb("LE.up_conv1.0")?,
            cb("LE.up_conv2.0")?,
            cb("LE.up_conv3.0")?,
        ];

        Ok(HdrItm {
            cc_convs,
            cc_ins,
            cc_conv_out: cb("AGCM.classifier.model.20")?,
            lin_scale,
            lin_shift,
            agcm_convs,
            le_conv_first: cb("LE.conv_first")?,
            le_sft1: SftW::load(&mut cb, "LE.SFT_layer1")?,
            le_hr_conv1: cb("LE.HR_conv1")?,
            le_down: vec![
                cb("LE.down_conv1")?,
                cb("LE.down_conv2")?,
                cb("LE.down_conv3")?,
            ],
            le_cond_first,
            le_cond_nets,
            le_trunks,
            le_up,
            le_sft2: SftW::load(&mut cb, "LE.SFT_layer2")?,
            le_hr_conv2: cb("LE.HR_conv2")?,
            le_conv_last: cb("LE.conv_last")?,
        })
    }

    /// SDR sRGB [0,1] RGB（CHW）→ PQ BT.2020 [0,1]（CHW）
    pub fn forward(&self, x: &Tensor) -> Result<Tensor, String> {
        if x.h < 32 || x.w < 32 {
            return Err(format!(
                "输入过小（{}x{}），ITM 网络 3 级下采样需 ≥32px",
                x.w, x.h
            ));
        }
        if x.h % 8 != 0 || x.w % 8 != 0 {
            return Err(format!(
                "输入尺寸（{}x{}）需为 8 的倍数（3 级下采样跳连对齐）",
                x.w, x.h
            ));
        }

        // ---- AGCM 条件分支（Color_Condition）----
        let mut fea = x.clone();
        for (i, conv) in self.cc_convs.iter().enumerate() {
            fea = conv2d(&fea, conv, 1);
            fea = depthwise_avg_pool3s2(&fea);
            leaky_inplace(&mut fea, 0.2);
            if i < 4 {
                fea = instance_norm(&fea, &self.cc_ins[i]);
            }
        }
        // 末端 1×1 conv（128→cond_c）→ GAP → 条件向量（Dropout eval 恒等，跳过）
        let fea = conv2d(&fea, &self.cc_conv_out, 1);
        let cond_vec = gap(&fea);

        // ---- AGCM 调制主干 ----
        let s_first = linear(&cond_vec, &self.lin_scale[0]);
        let sh_first = linear(&cond_vec, &self.lin_shift[0]);
        let mut out = conv2d(x, &self.agcm_convs[0], 1);
        modulate(&mut out, &s_first, &sh_first);
        relu_inplace(&mut out);
        let s_hr = linear(&cond_vec, &self.lin_scale[1]);
        let sh_hr = linear(&cond_vec, &self.lin_shift[1]);
        out = conv2d(&out, &self.agcm_convs[1], 1);
        modulate(&mut out, &s_hr, &sh_hr);
        relu_inplace(&mut out);
        let s_last = linear(&cond_vec, &self.lin_scale[2]);
        let sh_last = linear(&cond_vec, &self.lin_shift[2]);
        out = conv2d(&out, &self.agcm_convs[2], 1);
        modulate(&mut out, &s_last, &sh_last);
        let agcm_out = out;

        // ---- LE 条件分支（cond_first → CondNet1-4，多尺度条件特征）----
        let mut cond_img = agcm_out.clone();
        for conv in &self.le_cond_first {
            cond_img = conv2d(&cond_img, conv, 1);
            leaky_inplace(&mut cond_img, 0.1);
        }
        let c1 = conv_chain(&cond_img, &self.le_cond_nets[0], [1, 1, 1]);
        let c2 = conv_chain(&cond_img, &self.le_cond_nets[1], [2, 1, 1]);
        let c3 = conv_chain(&cond_img, &self.le_cond_nets[2], [2, 2, 1]);
        let c4 = conv_chain(&cond_img, &self.le_cond_nets[3], [2, 2, 2]);

        // ---- LE 主干（U-Net；trunk 块序号 0..8 对应 trunk1..5）----
        let mut fea0 = conv2d(&agcm_out, &self.le_conv_first, 1);
        relu_inplace(&mut fea0);
        fea0 = sft_forward(&fea0, &c1, &self.le_sft1);
        fea0 = conv2d(&fea0, &self.le_hr_conv1, 1);
        relu_inplace(&mut fea0);

        let mut fea1 = conv2d(&fea0, &self.le_down[0], 2);
        relu_inplace(&mut fea1);
        fea1 = res_block_sft_forward(&fea1, &c2, &self.le_trunks[0]);
        let mut fea2 = conv2d(&fea1, &self.le_down[1], 2);
        relu_inplace(&mut fea2);
        fea2 = res_block_sft_forward(&fea2, &c3, &self.le_trunks[1]);
        let mut fea3 = conv2d(&fea2, &self.le_down[2], 2);
        relu_inplace(&mut fea3);
        // trunk3：4 块串联（每块已含块内残差），块后另有长跳 out = out + fea3
        let mut o = fea3.clone();
        for b in 0..TRUNK_BLOCKS[2] {
            o = res_block_sft_forward(&o, &c4, &self.le_trunks[2 + b]);
        }
        o = add(&o, &fea3);

        // up1（trunk4）→ up2（trunk5）
        o = conv2d(&o, &self.le_up[0], 1);
        o = pixel_shuffle2(&o);
        relu_inplace(&mut o);
        o = add(&o, &fea2);
        o = res_block_sft_forward(&o, &c3, &self.le_trunks[6]);
        o = conv2d(&o, &self.le_up[1], 1);
        o = pixel_shuffle2(&o);
        relu_inplace(&mut o);
        o = add(&o, &fea1);
        o = res_block_sft_forward(&o, &c2, &self.le_trunks[7]);
        o = conv2d(&o, &self.le_up[2], 1);
        o = pixel_shuffle2(&o);
        relu_inplace(&mut o);
        o = add(&o, &fea0);
        o = sft_forward(&o, &c1, &self.le_sft2);
        o = conv2d(&o, &self.le_hr_conv2, 1);
        relu_inplace(&mut o);
        o = conv2d(&o, &self.le_conv_last, 1);
        // 末端长跳残差：out = AGCM 输出图 + 增益
        o = add(&o, &agcm_out);
        Ok(o)
    }
}

// ============================ 基础算子（CPU） ============================

/// 全图 AI 增强（AI 档入口）：每次从磁盘加载权重（原行为保持）
pub fn enhance_image_nits(sdr: &[f32], w: usize, h: usize) -> Result<Vec<f32>, String> {
    let model = HdrItm::load(&default_model_path())?;
    enhance_image_nits_with(&model, sdr, w, h)
}

/// 全图 AI 增强（权重已加载变体）：调用方缓存 HdrItm 复用，避免重复解析 .pth
///
/// 输入/输出口径与 [`enhance_image_nits`] 完全一致。
pub fn enhance_image_nits_with(
    model: &HdrItm,
    sdr: &[f32],
    w: usize,
    h: usize,
) -> Result<Vec<f32>, String> {
    let np = w * h;
    if sdr.len() != np * 3 {
        return Err(format!("输入尺寸不符: {} != {}x{}x3", sdr.len(), w, h));
    }

    // 8 对齐补边（edge replicate；网络 3 级下采样 + 跳连要求 8 的倍数）
    let pw = w.div_ceil(8) * 8;
    let ph = h.div_ceil(8) * 8;
    let mut pin = vec![0.0f32; pw * ph * 3];
    for c in 0..3 {
        for y in 0..ph {
            let sy = y.min(h - 1);
            for x in 0..pw {
                let sx = x.min(w - 1);
                pin[c * pw * ph + y * pw + x] = sdr[c * np + sy * w + sx];
            }
        }
    }

    let mut out = vec![0.0f32; pw * ph * 3];
    let gamut = crate::color::bt2020_to_bt709();
    for (ty, th) in tile_spans(ph) {
        for (tx, tw) in tile_spans(pw) {
            // 块输入提取
            let mut tile = Tensor::new(3, th, tw);
            for c in 0..3 {
                for y in 0..th {
                    let src = c * pw * ph + (ty + y) * pw + tx;
                    tile.data[y * tw..y * tw + tw].copy_from_slice(&pin[src..src + tw]);
                }
            }
            let pq = model.forward(&tile)?;
            // 逆 PQ → BT.2020 线性 nits → BT.709 nits，写回有效区（重叠区边缘除外）
            for y in 0..th {
                let py = ty + y;
                for x in 0..tw {
                    let lin = [
                        crate::color::pq_eotf(pq.at(0, y, x).clamp(0.0, 1.0)) * 10000.0,
                        crate::color::pq_eotf(pq.at(1, y, x).clamp(0.0, 1.0)) * 10000.0,
                        crate::color::pq_eotf(pq.at(2, y, x).clamp(0.0, 1.0)) * 10000.0,
                    ];
                    let (r, g, b) = gamut.apply(lin[0], lin[1], lin[2]);
                    let o = py * pw + tx + x;
                    out[o] = r.max(0.0);
                    out[pw * ph + o] = g.max(0.0);
                    out[2 * pw * ph + o] = b.max(0.0);
                }
            }
        }
    }

    // 裁掉补边
    let mut res = vec![0.0f32; np * 3];
    for c in 0..3 {
        for y in 0..h {
            res[c * np + y * w..c * np + y * w + w]
                .copy_from_slice(&out[c * pw * ph + y * pw..c * pw * ph + y * pw + w]);
        }
    }
    Ok(res)
}

/// 分块起止（首尾块贴边，中间块留 OVERLAP 重叠）
fn tile_spans(total: usize) -> Vec<(usize, usize)> {
    if total <= TILE {
        return vec![(0, total)];
    }
    let stride = TILE - 2 * OVERLAP;
    let mut spans = Vec::new();
    let mut s = 0;
    while s + TILE < total {
        spans.push((s, TILE));
        s += stride;
    }
    spans.push((total - TILE, TILE));
    spans
}

/// 2D 卷积（zero-pad，3×3→pad1 / 1×1→pad0；stride 1 或 2）
fn conv2d(x: &Tensor, cb: &CB, stride: usize) -> Tensor {
    let (oc, ic, k) = (cb.oc, cb.ic, cb.k);
    let pad = k / 2;
    let oh = (x.h + 2 * pad - k) / stride + 1;
    let ow = (x.w + 2 * pad - k) / stride + 1;
    let mut out = Tensor::new(oc, oh, ow);
    for c in 0..oc {
        let w_base = c * ic * k * k;
        let bias = cb.b[c];
        for y in 0..oh {
            for ox in 0..ow {
                let mut acc = bias;
                for ic_i in 0..ic {
                    let w_ch = w_base + ic_i * k * k;
                    for ky in 0..k {
                        let sy = y * stride + ky;
                        let sy = if sy >= pad && sy < pad + x.h {
                            sy - pad
                        } else {
                            continue;
                        };
                        for kx in 0..k {
                            let sx = ox * stride + kx;
                            let sx = if sx >= pad && sx < pad + x.w {
                                sx - pad
                            } else {
                                continue;
                            };
                            acc += cb.w[w_ch + ky * k + kx] * x.at(ic_i, sy, sx);
                        }
                    }
                }
                out.set(c, y, ox, acc);
            }
        }
    }
    out
}

fn leaky_inplace(x: &mut Tensor, slope: f32) {
    for v in x.data.iter_mut() {
        if *v < 0.0 {
            *v *= slope;
        }
    }
}

fn relu_inplace(x: &mut Tensor) {
    for v in x.data.iter_mut() {
        if *v < 0.0 {
            *v = 0.0;
        }
    }
}

/// InstanceNorm2d（逐通道空间统计，affine）
fn instance_norm(x: &Tensor, gb: &GB) -> Tensor {
    let mut out = x.clone();
    for c in 0..x.c {
        let n = (x.h * x.w) as f32;
        let (mut sum, mut sum_sq) = (0.0f32, 0.0f32);
        for y in 0..x.h {
            for xx in 0..x.w {
                let v = x.at(c, y, xx);
                sum += v;
                sum_sq += v * v;
            }
        }
        let mean = sum / n;
        let var = (sum_sq / n - mean * mean).max(0.0);
        let inv = 1.0 / (var + IN_EPS).sqrt();
        for y in 0..x.h {
            for xx in 0..x.w {
                let v = (x.at(c, y, xx) - mean) * inv * gb.g[c] + gb.b[c];
                out.set(c, y, xx, v);
            }
        }
    }
    out
}

/// Global Average Pool → [C]
fn gap(x: &Tensor) -> Vec<f32> {
    (0..x.c)
        .map(|c| {
            let n = (x.h * x.w) as f32;
            let mut s = 0.0f32;
            for y in 0..x.h {
                for xx in 0..x.w {
                    s += x.at(c, y, xx);
                }
            }
            s / n
        })
        .collect()
}

/// Linear（weight [out,in]，bias [out]）
fn linear(v: &[f32], cb: &CB) -> Vec<f32> {
    let (o, i) = (cb.oc, cb.ic);
    (0..o)
        .map(|oi| {
            let base = oi * i;
            let mut acc = cb.b[oi];
            for ii in 0..i {
                acc += cb.w[base + ii] * v[ii];
            }
            acc
        })
        .collect()
}

/// PixelShuffle(2)：[C*4, H, W] → [C, 2H, 2W]（PyTorch 语义：out[c, 2y+r, 2x+col] = in[c*4 + r*2 + col, y, x]）
fn pixel_shuffle2(x: &Tensor) -> Tensor {
    let c = x.c / 4;
    let (h, w) = (x.h * 2, x.w * 2);
    let mut out = Tensor::new(c, h, w);
    for src_c in 0..x.c {
        let oc = src_c / 4;
        let r = (src_c % 4) / 2;
        let col = src_c % 2;
        for y in 0..x.h {
            for xx in 0..x.w {
                out.set(oc, y * 2 + r, xx * 2 + col, x.at(src_c, y, xx));
            }
        }
    }
    out
}

fn modulate(x: &mut Tensor, s: &[f32], sh: &[f32]) {
    for c in 0..x.c {
        let sc = 1.0 + s[c];
        let shc = sh[c];
        for y in 0..x.h {
            for xx in 0..x.w {
                let v = x.at(c, y, xx) * sc + shc;
                x.set(c, y, xx, v);
            }
        }
    }
}

fn add(a: &Tensor, b: &Tensor) -> Tensor {
    let mut o = a.clone();
    for (v, bv) in o.data.iter_mut().zip(b.data.iter()) {
        *v += bv;
    }
    o
}

/// SFTLayer：scale/shift = Conv1(leaky(Conv0(cond),0.1))；返回 fea·(scale+1)+shift
fn sft_forward(fea: &Tensor, cond: &Tensor, s: &SftW) -> Tensor {
    let mut t0 = conv2d(cond, &s.s0, 1);
    leaky_inplace(&mut t0, 0.1);
    let scale = conv2d(&t0, &s.s1, 1);
    let mut h0 = conv2d(cond, &s.h0, 1);
    leaky_inplace(&mut h0, 0.1);
    let shift = conv2d(&h0, &s.h1, 1);
    let mut o = fea.clone();
    for ((fv, sv), hv) in o
        .data
        .iter_mut()
        .zip(scale.data.iter())
        .zip(shift.data.iter())
    {
        *fv = *fv * (sv + 1.0) + hv;
    }
    o
}

/// ResBlock_with_SFT：sft1 → conv1+ReLU → sft2 → conv2 → 残差
fn res_block_sft_forward(fea: &Tensor, cond: &Tensor, t: &ResSft) -> Tensor {
    let mut f = sft_forward(fea, cond, &t.sft1);
    f = conv2d(&f, &t.conv1, 1);
    relu_inplace(&mut f);
    f = sft_forward(&f, cond, &t.sft2);
    f = conv2d(&f, &t.conv2, 1);
    let mut o = f;
    for (v, bv) in o.data.iter_mut().zip(fea.data.iter()) {
        *v += bv;
    }
    o
}

/// 3 层 Sequential conv（LeakyReLU 0.1 夹层；逐层 stride，对应 CondNet1-4）
fn conv_chain(x: &Tensor, ws: &[CB], strides: [usize; 3]) -> Tensor {
    let a = conv2d(x, &ws[0], strides[0]);
    let mut a = a;
    leaky_inplace(&mut a, 0.1);
    let b = conv2d(&a, &ws[1], strides[1]);
    let mut b = b;
    leaky_inplace(&mut b, 0.1);
    conv2d(&b, &ws[2], strides[2])
}

/// depthwise AvgPool 3×3 s2（count_include_pad=True → zero-pad 除 9）
fn depthwise_avg_pool3s2(x: &Tensor) -> Tensor {
    let mut out = Tensor::new(x.c, x.h.div_ceil(2), x.w.div_ceil(2));
    for c in 0..x.c {
        for oy in 0..out.h {
            for ox in 0..out.w {
                let mut acc = 0.0f32;
                for dy in 0..3 {
                    for dx in 0..3 {
                        let sy = oy * 2 + dy;
                        let sx = ox * 2 + dx;
                        if sy < x.h && sx < x.w {
                            acc += x.at(c, sy, sx);
                        }
                    }
                }
                out.set(c, oy, ox, acc / 9.0);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dump_weights() {
        let pth = std::path::Path::new("e:/jietu/app/src-tauri/models/itm/Ensemble_AGCM_LE.pth");
        if !pth.exists() {
            eprintln!("[itm] 权重不存在，跳过");
            return;
        }
        let mut pth = PthFile::open(pth).unwrap();
        let mut names: Vec<String> = pth.names().into_iter().cloned().collect();
        names.sort();
        eprintln!("[itm] 变量总数 = {}", names.len());
        for n in &names {
            if let Some(t) = pth.tensor(n) {
                eprintln!("  {} {:?}", n, t.shape);
            }
        }
    }

    #[test]
    fn test_itm_load_and_forward() {
        let pth = std::path::Path::new("e:/jietu/app/src-tauri/models/itm/Ensemble_AGCM_LE.pth");
        if !pth.exists() {
            eprintln!("[itm] 权重不存在，跳过");
            return;
        }
        let m = HdrItm::load(pth).expect("加载失败");
        let (w, h) = (64usize, 64usize);

        // 真实照片（自然图像分布内）；无壁纸则退回平滑渐变
        let img_path = std::path::Path::new(r"C:\Windows\Web\Wallpaper\ThemeA\img20.jpg");
        let x = if img_path.exists() {
            let img = image::ImageReader::open(img_path)
                .expect("打开图片失败")
                .decode()
                .expect("解码失败")
                .to_rgb8();
            let small =
                image::imageops::resize(&img, 64, 64, image::imageops::FilterType::Lanczos3);
            let mut x = Tensor::new(3, h, w);
            for y in 0..h {
                for xx in 0..w {
                    let p = small.get_pixel(xx as u32, y as u32);
                    x.set(0, y, xx, p[0] as f32 / 255.0);
                    x.set(1, y, xx, p[1] as f32 / 255.0);
                    x.set(2, y, xx, p[2] as f32 / 255.0);
                }
            }
            x
        } else {
            eprintln!("[itm] 无测试壁纸，退回平滑渐变");
            let mut x = Tensor::new(3, h, w);
            for y in 0..h {
                for xx in 0..w {
                    let v = (xx + y) as f32 / (w + h) as f32;
                    x.set(0, y, xx, v);
                    x.set(1, y, xx, (v * 0.8 + 0.1).clamp(0.0, 1.0));
                    x.set(2, y, xx, 1.0 - v);
                }
            }
            x
        };

        let out = m.forward(&x).expect("前向失败");
        assert_eq!((out.c, out.h, out.w), (3, h, w), "输出尺寸");
        // 输出为 PQ 编码 [0,1]：绝大多数像素应在 [0,1] 内（少量越界 clamp 由编码端处理）
        let mut in_range = 0usize;
        let mut mn = f32::MAX;
        let mut mx = f32::MIN;
        for v in &out.data {
            mn = mn.min(*v);
            mx = mx.max(*v);
            if (0.0..=1.0).contains(v) {
                in_range += 1;
            }
        }
        eprintln!(
            "[itm] 输出范围 [{:.4},{:.4}]，[0,1] 内比例 {:.1}%",
            mn,
            mx,
            in_range as f32 * 100.0 / out.data.len() as f32
        );
        assert!(
            in_range as f32 > out.data.len() as f32 * 0.95,
            "输出 95%+ 应在 [0,1]"
        );
        // 非 identity：输出应与输入明显不同（PQ 曲线 + 色彩处理）
        let mut diff = 0.0f32;
        for (o, i) in out.data.iter().zip(x.data.iter()) {
            diff += (o - i).abs();
        }
        let mean_diff = diff / out.data.len() as f32;
        eprintln!("[itm] 输入输出平均差 {:.4}", mean_diff);
        assert!(mean_diff > 0.01, "输出应非恒等（平均差 {:.4}）", mean_diff);
    }

    #[test]
    fn test_enhance_image_nits() {
        let pth = default_model_path();
        if !pth.exists() {
            eprintln!("[itm] 权重不存在，跳过");
            return;
        }
        let img_path = std::path::Path::new(r"C:\Windows\Web\Wallpaper\ThemeA\img20.jpg");
        if !img_path.exists() {
            eprintln!("[itm] 无测试壁纸，跳过");
            return;
        }
        let img = image::ImageReader::open(img_path)
            .expect("打开图片失败")
            .decode()
            .expect("解码失败")
            .to_rgb8();
        let (w, h) = (48usize, 40usize); // 非 8 倍数（验证补边）
        let small = image::imageops::resize(&img, 48, 40, image::imageops::FilterType::Lanczos3);
        let mut sdr = vec![0.0f32; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let p = small.get_pixel(x as u32, y as u32);
                for c in 0..3 {
                    sdr[c * w * h + y * w + x] = p[c] as f32 / 255.0;
                }
            }
        }
        let nits = enhance_image_nits(&sdr, w, h).expect("AI 推理失败");
        assert_eq!(nits.len(), w * h * 3);
        let mn = nits.iter().fold(f32::MAX, |a, &v| a.min(v));
        let mx = nits.iter().fold(f32::MIN, |a, &v| a.max(v));
        eprintln!("[itm] AI nits 范围 [{:.1},{:.1}] nits", mn, mx);
        assert!(mn >= 0.0, "nits 非负");
        // PQ 域最大 10000nits；输出不应疯狂越界
        assert!(mx <= 10001.0, "nits 超 PQ 上限: {}", mx);
        // 中间调不应被推到极端（自然图多数像素 < 1000nits）
        let over = nits.iter().filter(|&&v| v > 1000.0).count();
        eprintln!(
            "[itm] >1000nits 占比 {:.1}%",
            over as f32 * 100.0 / nits.len() as f32
        );
        assert!(
            (over as f32) < (nits.len() as f32) * 0.5,
            "过半像素超 1000nits，疑似数值异常"
        );
    }
}
