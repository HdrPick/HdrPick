//! 网络拓扑：从 NetDef 构建可执行模型（blob 槽位 DAG + 形状推算）
//!
//! P5 扩展：支持 SE 残差网络（upresnet10 / cunet）的全部层类型——
//! Conv（含 stride-2 下采样）/ Deconv（含 leaky）/ GlobalAvgPool /
//! InnerProduct（转 1×1 conv）/ Sigmoid / Scale / Axpy / CropCenter / Eltwise。
//! Split/Flatten 编译为槽位别名（零开销）。
//!
//! 执行模型：blob 槽位 DAG——每层带 bottom/top 槽位索引，
//! 槽 0 = 网络输入，运行时用张量竞技场（CPU）/ 纹理竞技场（GPU）求值。

use super::caffemodel::{ConvParam, LayerDef, NetDef};

/// 形状推算探针尺寸（须为偶数，覆盖 stride-2 路径）
const PROBE: usize = 256;

/// 模型家族（对应原版模型目录名）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModelKind {
    Upconv7AnimeStyleArtRgb,
    Upconv7Photo,
    Upresnet10,
    Cunet,
    /// Real-ESRGAN 通用 ×4（RRDBNet 23 块，真实退化训练）
    RealEsrganX4,
    /// Real-ESRGAN 动漫 ×4（RRDBNet 6 块）
    RealEsrganAnime6B,
    AnimeStyleArt, // Y 通道（P3）
    Photo,         // Y 通道（P3）
    Ukbench,       // P3
}

impl ModelKind {
    /// 原版模型目录名
    pub fn dir_name(self) -> &'static str {
        match self {
            ModelKind::Upconv7AnimeStyleArtRgb => "upconv_7_anime_style_art_rgb",
            ModelKind::Upconv7Photo => "upconv_7_photo",
            ModelKind::Upresnet10 => "upresnet10",
            ModelKind::Cunet => "cunet",
            ModelKind::RealEsrganX4 | ModelKind::RealEsrganAnime6B => "RealESRGAN",
            ModelKind::AnimeStyleArt => "anime_style_art",
            ModelKind::Photo => "photo",
            ModelKind::Ukbench => "ukbench",
        }
    }
    /// 是否 Real-ESRGAN 家族（.pth 权重加载路径）
    pub fn is_realesrgan(self) -> bool {
        matches!(self, ModelKind::RealEsrganX4 | ModelKind::RealEsrganAnime6B)
    }
    pub fn display_name(self) -> &'static str {
        match self {
            ModelKind::Upconv7AnimeStyleArtRgb => "Upconv7 动漫(RGB)",
            ModelKind::Upconv7Photo => "Upconv7 照片(RGB)",
            ModelKind::Upresnet10 => "UpResNet10",
            ModelKind::Cunet => "CUnet 动漫(最高质量)",
            ModelKind::RealEsrganX4 => "Real-ESRGAN 照片(推荐)",
            ModelKind::RealEsrganAnime6B => "Real-ESRGAN 动漫",
            ModelKind::AnimeStyleArt => "动漫(Y通道)",
            ModelKind::Photo => "照片(Y通道)",
            ModelKind::Ukbench => "Ukbench 照片",
        }
    }
    /// 模型文件名（scale 模式）
    pub fn scale_file(self) -> &'static str {
        match self {
            ModelKind::RealEsrganX4 => "RealESRGAN_x4plus.pth",
            ModelKind::RealEsrganAnime6B => "RealESRGAN_x4plus_anime_6B.pth",
            _ => "scale2.0x_model.json.caffemodel",
        }
    }
    /// 降噪+放大合并模型文件名（upconv_7/upresnet10/cunet 家族有；
    /// 运行时文件不存在则调用方回落 scale 模型）
    pub fn noise_scale_file(self, level: u32) -> String {
        format!("noise{}_scale2.0x_model.json.caffemodel", level)
    }
    /// 该家族是否有合并降噪模型（决定 noise 模式可用性）
    pub fn has_noise_scale_model(self) -> bool {
        matches!(
            self,
            ModelKind::Upconv7AnimeStyleArtRgb
                | ModelKind::Upconv7Photo
                | ModelKind::Upresnet10
                | ModelKind::Cunet
        )
    }
    /// 噪声模型文件名
    pub fn noise_file(self, level: u32) -> String {
        format!("noise{}_model.json.caffemodel", level)
    }
    /// 是否有独立噪声模型（cunet 有 noise-only 模型，scale 模型不复用）
    /// Real-ESRGAN 无降噪分档（x4 模型本身即"修复+放大"一体）
    pub fn has_noise_models(self) -> bool {
        !matches!(self, ModelKind::Ukbench) && !self.is_realesrgan()
    }
}

/// 执行层（blob 槽位 DAG：bottom/top 为槽位索引）
#[derive(Clone, Debug)]
pub enum ExecLayer {
    /// 卷积（任意 kernel/stride，带 bias + leaky ReLU）
    Conv {
        weight: Vec<f32>, // [out_ch, in_ch, kh, kw]
        bias: Vec<f32>,   // [out_ch]
        in_ch: usize,
        out_ch: usize,
        kernel: usize,
        pad: usize,
        stride: usize,
        dilation: usize,
        leaky_slope: f32,
        bottom: usize,
        top: usize,
    },
    /// 反卷积（上采样/像素重排，带 bias + leaky ReLU）
    Deconv {
        weight: Vec<f32>, // [in_ch, out_ch, kh, kw]（Caffe 存储顺序）
        bias: Vec<f32>,
        in_ch: usize,
        out_ch: usize,
        kernel: usize,
        stride: usize,
        pad: usize,
        leaky_slope: f32,
        bottom: usize,
        top: usize,
    },
    /// 全局平均池化（SE squeeze）：C×H×W → C×1×1
    GlobalAvgPool { bottom: usize, top: usize },
    /// 逐元素 sigmoid（SE excite 激活）
    Sigmoid { bottom: usize, top: usize },
    /// 通道缩放（SE gate）：data × vec[c]（cunet Scale 层）
    Scale { data: usize, vec: usize, top: usize },
    /// out = alpha[c]·x + y（upresnet10 Axpy 层：alpha 为 1×1×C 广播）
    Axpy {
        alpha: usize,
        x: usize,
        y: usize,
        top: usize,
    },
    /// 中心裁剪：每边去 crop_h/crop_w 像素（upresnet10 CropCenter）
    CropCenter {
        crop_h: usize,
        crop_w: usize,
        bottom: usize,
        top: usize,
    },
    /// 参考锚定裁剪（cunet Caffe Crop）：out[y,x] = in[y+off, x+off]，
    /// 输出尺寸 = reference 形状（奇数中间尺寸下非对称——与原版语义一致）
    Crop {
        offset: usize,
        reference: usize,
        bottom: usize,
        top: usize,
    },
    /// 逐元素相加（残差 SUM）
    EltwiseAdd { a: usize, b: usize, top: usize },
    /// 通道维拼接（RRDB 密集连接；GPU 每 pass 最多 3 输入——构建期自动拆分）
    Concat { bottoms: Vec<usize>, top: usize },
    /// 最近邻 ×2 上采样（RRDBNet 上采样：F.interpolate(nearest) → conv）
    NearestUp2 { bottom: usize, top: usize },
    /// out = β·a + b（RDB 残差缩放，β=0.2）
    ScaledAdd {
        a: usize,
        b: usize,
        beta: f32,
        top: usize,
    },
}

impl ExecLayer {
    /// 所有 bottom 槽位
    pub fn bottoms(&self) -> Vec<usize> {
        match self {
            ExecLayer::Conv { bottom, .. } | ExecLayer::Deconv { bottom, .. } => vec![*bottom],
            ExecLayer::GlobalAvgPool { bottom, .. } | ExecLayer::Sigmoid { bottom, .. } => {
                vec![*bottom]
            }
            ExecLayer::Scale { data, vec, .. } => vec![*data, *vec],
            ExecLayer::Axpy { alpha, x, y, .. } => vec![*alpha, *x, *y],
            ExecLayer::CropCenter { bottom, .. } => vec![*bottom],
            // reference 参与 bottoms（CPU 取运行时尺寸 / GPU 纹理生命周期）
            ExecLayer::Crop {
                bottom, reference, ..
            } => vec![*bottom, *reference],
            ExecLayer::EltwiseAdd { a, b, .. } => vec![*a, *b],
            ExecLayer::Concat { bottoms, .. } => bottoms.clone(),
            ExecLayer::NearestUp2 { bottom, .. } => vec![*bottom],
            ExecLayer::ScaledAdd { a, b, .. } => vec![*a, *b],
        }
    }
    /// top 槽位
    pub fn top(&self) -> usize {
        match self {
            ExecLayer::Conv { top, .. }
            | ExecLayer::Deconv { top, .. }
            | ExecLayer::GlobalAvgPool { top, .. }
            | ExecLayer::Sigmoid { top, .. }
            | ExecLayer::Scale { top, .. }
            | ExecLayer::Axpy { top, .. }
            | ExecLayer::CropCenter { top, .. }
            | ExecLayer::Crop { top, .. }
            | ExecLayer::EltwiseAdd { top, .. }
            | ExecLayer::Concat { top, .. }
            | ExecLayer::NearestUp2 { top, .. }
            | ExecLayer::ScaledAdd { top, .. } => *top,
        }
    }
}

/// 输出形状推算（单源：构建期探针与运行期（GPU 会话）共用）
///
/// `shapes` 需已含本层全部 bottom 的形状
fn layer_out_shape(
    l: &ExecLayer,
    shapes: &[(usize, usize, usize)],
) -> Result<(usize, usize, usize), String> {
    let get = |s: usize| shapes.get(s).copied().ok_or("bottom 槽位缺失");
    Ok(match l {
        ExecLayer::Conv {
            bottom,
            out_ch,
            kernel,
            pad,
            stride,
            dilation,
            ..
        } => {
            let (iw, ih, _) = get(*bottom)?;
            // out = floor((in + 2p - d(k-1) - 1)/s) + 1
            let ow = (iw + 2 * pad).saturating_sub((kernel - 1) * dilation + 1) / stride + 1;
            let oh = (ih + 2 * pad).saturating_sub((kernel - 1) * dilation + 1) / stride + 1;
            (ow, oh, *out_ch)
        }
        ExecLayer::Deconv {
            bottom,
            out_ch,
            kernel,
            stride,
            pad,
            ..
        } => {
            let (iw, ih, _) = get(*bottom)?;
            let ow = (iw - 1) * stride + kernel - 2 * pad;
            let oh = (ih - 1) * stride + kernel - 2 * pad;
            (ow, oh, *out_ch)
        }
        ExecLayer::GlobalAvgPool { bottom, .. } => {
            let (_, _, c) = get(*bottom)?;
            (1, 1, c)
        }
        ExecLayer::Sigmoid { bottom, .. } => get(*bottom)?,
        ExecLayer::Scale { data, .. } => get(*data)?,
        ExecLayer::Axpy { y, .. } => get(*y)?,
        ExecLayer::CropCenter {
            crop_h,
            crop_w,
            bottom,
            ..
        } => {
            let (iw, ih, c) = get(*bottom)?;
            if ih < 2 * crop_h || iw < 2 * crop_w {
                return Err(format!(
                    "CropCenter 裁剪越界: in {iw}x{ih} crop {crop_w}x{crop_h}"
                ));
            }
            (iw - 2 * crop_w, ih - 2 * crop_h, c)
        }
        ExecLayer::Crop {
            offset,
            reference,
            bottom,
            ..
        } => {
            let (dw, dh, dc) = get(*bottom)?;
            let (rw, rh, _) = get(*reference)?;
            if dh < *offset + rh || dw < *offset + rw {
                return Err(format!(
                    "Crop 越界: in {dw}x{dh} ref {rw}x{rh} offset {}",
                    *offset
                ));
            }
            (rw, rh, dc)
        }
        ExecLayer::EltwiseAdd { a, .. } => get(*a)?,
        ExecLayer::Concat { bottoms, .. } => {
            // 通道求和，空间取首输入
            let (w0, h0, _) = get(bottoms[0])?;
            let c: usize = bottoms
                .iter()
                .map(|b| get(*b).map(|s| s.2))
                .collect::<Result<Vec<_>, _>>()?
                .iter()
                .sum();
            for b in bottoms {
                let (wb, hb, _) = get(*b)?;
                if (wb, hb) != (w0, h0) {
                    return Err(format!("Concat 空间尺寸不一致 {w0}x{h0} vs {wb}x{hb}"));
                }
            }
            (w0, h0, c)
        }
        ExecLayer::NearestUp2 { bottom, .. } => {
            let (w, h, c) = get(*bottom)?;
            (w * 2, h * 2, c)
        }
        ExecLayer::ScaledAdd { a, .. } => get(*a)?,
    })
}

/// 可执行 waifu2x 模型
#[derive(Clone)]
pub struct WaifuModel {
    /// 执行层序列（NetDef 拓扑序）
    pub layers: Vec<ExecLayer>,
    /// 输入通道数（RGB 模型=3）
    pub in_ch: usize,
    /// 原版 net_offset（info.json 的 offset 字段）：
    /// 每边 pad 量 = offset，输出有效区起点 = offset（中心裁剪）。
    /// 注意与数学感受野半宽不同——原版 pad 感受野的 2 倍以保证边缘像素
    /// 不受 replicate 污染。
    pub net_offset: usize,
    /// 内部放大倍率（Π deconv stride / Π conv stride）
    pub inner_scale: usize,
    pub kind: ModelKind,
    /// blob 槽位总数（输入 = 槽 0）
    pub n_slots: usize,
    /// 输出槽位（最后一层的 top）
    pub output_slot: usize,
}

impl WaifuModel {
    /// 从 NetDef 构建（blob 槽位 DAG；Split/Flatten → 别名，ReLU 合并进前一 conv/deconv/fc）
    pub fn build(kind: ModelKind, net: &NetDef) -> Result<Self, String> {
        // 输入通道：第一个 conv 的 blob[0] shape = [out, in, kh, kw]
        let first = net
            .layers
            .iter()
            .find(|l| l.layer_type == "Convolution")
            .ok_or("无 Convolution")?;
        let w0 = first.blobs.first().ok_or("第一层无权重 blob")?;
        if w0.shape.len() != 4 {
            return Err(format!("第一层权重形状异常 {:?}", w0.shape));
        }
        let in_ch = w0.shape[1];

        // name → ReLU 斜率（ReLU 层与生产层同名配对：bottom = 生产层 top，in-place）
        let relu_slopes: std::collections::HashMap<String, f32> = net
            .layers
            .iter()
            .filter(|l| l.layer_type == "ReLU")
            .filter_map(|l| {
                let bottom = l.bottoms.first()?;
                Some((bottom.clone(), l.negative_slope))
            })
            .collect();

        let mut layers: Vec<ExecLayer> = Vec::new();
        let mut shapes: Vec<(usize, usize, usize)> = Vec::new();
        let mut slot_of: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        // inner_scale = Π deconv stride / Π conv stride（分子/分母分离，避免整数除法截断）
        let mut scale_num: usize = 1;
        let mut scale_den: usize = 1;

        // 无后续 ReLU 的层 → slope 1.0（恒等透传）；有 ReLU → 其 negative_slope（0=清零负值）
        // 关键：fc2/末层 conv 等无 ReLU 层的负值必须原样进入 Sigmoid/输出，不得清零
        let slope_of = |top: &str| -> f32 { relu_slopes.get(top).copied().unwrap_or(1.0) };
        // 层 top 名（&str 视图；缺省空串）
        fn top_name(l: &LayerDef) -> &str {
            l.tops.first().map(|s| s.as_str()).unwrap_or("")
        }

        for l in &net.layers {
            match l.layer_type.as_str() {
                "Input" => {
                    if let Some(top) = l.tops.first() {
                        if !slot_of.contains_key(top) {
                            // 输入 = 槽 0
                            debug_assert_eq!(shapes.len(), 0, "Input 必须是首层");
                            shapes.push((PROBE, PROBE, in_ch));
                            slot_of.insert(top.clone(), 0);
                        }
                    }
                }
                "Convolution" => {
                    let bottom = *slot_of
                        .get(l.bottoms.first().ok_or("conv 无 bottom")?)
                        .ok_or_else(|| format!("conv {} bottom 未定义", l.name))?;
                    let top = shapes.len();
                    let exec = set_slots(Self::make_conv(l, slope_of(top_name(l)))?, bottom, top);
                    scale_den *= exec_stride(&exec);
                    let shape = layer_out_shape(&exec, &shapes)
                        .map_err(|e| format!("conv {}: {}", l.name, e))?;
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "Deconvolution" => {
                    let bottom = *slot_of
                        .get(l.bottoms.first().ok_or("deconv 无 bottom")?)
                        .ok_or_else(|| format!("deconv {} bottom 未定义", l.name))?;
                    let top = shapes.len();
                    let exec = set_slots(Self::make_deconv(l, slope_of(top_name(l)))?, bottom, top);
                    scale_num *= exec_stride(&exec);
                    let shape = layer_out_shape(&exec, &shapes)
                        .map_err(|e| format!("deconv {}: {}", l.name, e))?;
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "InnerProduct" => {
                    // 1×1×C 上的全连接 ≡ 1×1 卷积（GAP 输出空间维必为 1）
                    let bottom = *slot_of
                        .get(l.bottoms.first().ok_or("fc 无 bottom")?)
                        .ok_or_else(|| format!("fc {} bottom 未定义", l.name))?;
                    let w = l.blobs.first().ok_or("fc 无权重")?;
                    if w.shape.len() != 2 {
                        return Err(format!("fc {} 权重非 2D: {:?}", l.name, w.shape));
                    }
                    let (out_c, in_c) = (w.shape[0], w.shape[1]);
                    let bias = l
                        .blobs
                        .get(1)
                        .map(|b| b.data.clone())
                        .unwrap_or_else(|| vec![0.0; out_c]);
                    // 【调试】FC_VARIANT：0=[out,in] 标准布局 1=[in,out] 转置读法
                    const FC_VARIANT: u8 = 0;
                    let weight = if FC_VARIANT == 1 {
                        // blob 存储 [in][out] → 转成 conv 布局 [out][in]
                        let mut wt = vec![0.0f32; out_c * in_c];
                        for ic in 0..in_c {
                            for oc in 0..out_c {
                                wt[oc * in_c + ic] = w.data[ic * out_c + oc];
                            }
                        }
                        wt
                    } else {
                        w.data.clone()
                    };
                    let exec = ExecLayer::Conv {
                        weight,
                        bias,
                        in_ch: in_c,
                        out_ch: out_c,
                        kernel: 1,
                        pad: 0,
                        stride: 1,
                        dilation: 1,
                        leaky_slope: slope_of(top_name(l)),
                        bottom,
                        top: 0, // 占位，下方回填
                    };
                    let shape = layer_out_shape(&exec, &shapes)
                        .map_err(|e| format!("fc {}: {}", l.name, e))?;
                    if shape.0 != 1 || shape.1 != 1 {
                        return Err(format!(
                            "fc {} 输入非 1×1（{}×{}）——InnerProduct 仅支持 GAP 后",
                            l.name, shape.0, shape.1
                        ));
                    }
                    let top = shapes.len();
                    let exec = set_top(exec, top);
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "ReLU" => {
                    // 已合并进生产层（leaky_slope）；非 in-place（top 名 ≠ bottom 名）时
                    // 需注册别名：后续层引用的是 ReLU 的 top 名
                    if let (Some(b), Some(t)) = (l.bottoms.first(), l.tops.first()) {
                        if b != t {
                            if let Some(s) = slot_of.get(b) {
                                slot_of.insert(t.clone(), *s);
                            }
                        }
                    }
                }
                "Sigmoid" => {
                    let bottom = *slot_of
                        .get(l.bottoms.first().ok_or("sigmoid 无 bottom")?)
                        .ok_or("sigmoid bottom 未定义")?;
                    let exec = ExecLayer::Sigmoid { bottom, top: 0 };
                    let shape = layer_out_shape(&exec, &shapes)?;
                    let top = shapes.len();
                    let exec = set_top(exec, top);
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "Pooling" => {
                    let p = l.pooling.ok_or("Pooling 无参数")?;
                    if p.pool != 1 || !p.global {
                        return Err(format!(
                            "Pooling {} 非 global AVE（pool={} global={}）",
                            l.name, p.pool, p.global
                        ));
                    }
                    let bottom = *slot_of
                        .get(l.bottoms.first().ok_or("pool 无 bottom")?)
                        .ok_or("pool bottom 未定义")?;
                    let exec = ExecLayer::GlobalAvgPool { bottom, top: 0 };
                    let shape = layer_out_shape(&exec, &shapes)?;
                    let top = shapes.len();
                    let exec = set_top(exec, top);
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "Flatten" => {
                    // 1×1×C → C：张量表示不变（别名）
                    if let (Some(b), Some(t)) = (l.bottoms.first(), l.tops.first()) {
                        let s = *slot_of.get(b).ok_or("flatten bottom 未定义")?;
                        slot_of.insert(t.clone(), s);
                    }
                }
                "Split" => {
                    // 扇出别名（零开销）
                    let src = l.bottoms.first().and_then(|b| slot_of.get(b)).copied();
                    if let Some(src) = src {
                        for t in &l.tops {
                            slot_of.insert(t.clone(), src);
                        }
                    }
                }
                "CropCenter" => {
                    // 自定义层：offset[2]/offset[3] = H/W 每边裁剪量（[0,0,h,w]）
                    let cc = l.crop_center.as_ref().ok_or("CropCenter 无参数")?;
                    let crop_h = *cc.offsets.get(2).ok_or("CropCenter offset 缺 H")? as usize;
                    let crop_w = *cc.offsets.get(3).ok_or("CropCenter offset 缺 W")? as usize;
                    let bottom = *slot_of
                        .get(l.bottoms.first().ok_or("crop 无 bottom")?)
                        .ok_or("crop bottom 未定义")?;
                    let exec = ExecLayer::CropCenter {
                        crop_h,
                        crop_w,
                        bottom,
                        top: 0,
                    };
                    let shape = layer_out_shape(&exec, &shapes)
                        .map_err(|e| format!("crop {}: {}", l.name, e))?;
                    let top = shapes.len();
                    let exec = set_top(exec, top);
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "Crop" => {
                    // Caffe Crop：bottom0 裁到 bottom1 形状，axis=2 起、offset 锚定
                    // （奇数中间尺寸下非对称——保持原版语义，不做对称化）
                    let data = *slot_of
                        .get(l.bottoms.first().ok_or("Crop 无 bottom0")?)
                        .ok_or("Crop bottom0 未定义")?;
                    let r#ref = *slot_of
                        .get(l.bottoms.get(1).ok_or("Crop 无 bottom1")?)
                        .ok_or("Crop bottom1 未定义")?;
                    let cp = l.crop.as_ref().ok_or("Crop 无参数")?;
                    let exec = ExecLayer::Crop {
                        offset: cp.offset as usize,
                        reference: r#ref,
                        bottom: data,
                        top: 0,
                    };
                    let shape = layer_out_shape(&exec, &shapes)
                        .map_err(|e| format!("crop {}: {}", l.name, e))?;
                    let top = shapes.len();
                    let exec = set_top(exec, top);
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "Axpy" => {
                    // out = alpha[c]·x + y（alpha 1×1×C 广播；形状取 y）
                    let g = |n: usize| -> Result<usize, String> {
                        slot_of
                            .get(l.bottoms.get(n).ok_or("Axpy bottom 缺失")?)
                            .copied()
                            .ok_or_else(|| "Axpy bottom 未定义".to_string())
                    };
                    let alpha = g(0)?;
                    let x = g(1)?;
                    let y = g(2)?;
                    let exec = ExecLayer::Axpy {
                        alpha,
                        x,
                        y,
                        top: 0,
                    };
                    let shape = layer_out_shape(&exec, &shapes)?;
                    let top = shapes.len();
                    let exec = set_top(exec, top);
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "Scale" => {
                    // out = data × vec[c]
                    let data = *slot_of
                        .get(l.bottoms.first().ok_or("Scale 无 data")?)
                        .ok_or("Scale data 未定义")?;
                    let vec = *slot_of
                        .get(l.bottoms.get(1).ok_or("Scale 无 vec")?)
                        .ok_or("Scale vec 未定义")?;
                    let exec = ExecLayer::Scale { data, vec, top: 0 };
                    let shape = layer_out_shape(&exec, &shapes)?;
                    let top = shapes.len();
                    let exec = set_top(exec, top);
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                "Eltwise" => {
                    let a = *slot_of
                        .get(l.bottoms.first().ok_or("eltwise 无 bottom0")?)
                        .ok_or("eltwise bottom0 未定义")?;
                    let b = *slot_of
                        .get(l.bottoms.get(1).ok_or("eltwise 无 bottom1")?)
                        .ok_or("eltwise bottom1 未定义")?;
                    if shapes[a] != shapes[b] {
                        return Err(format!(
                            "eltwise {} 形状不一致 {:?} vs {:?}",
                            l.name, shapes[a], shapes[b]
                        ));
                    }
                    let exec = ExecLayer::EltwiseAdd { a, b, top: 0 };
                    let shape = layer_out_shape(&exec, &shapes)?;
                    let top = shapes.len();
                    let exec = set_top(exec, top);
                    shapes.push(shape);
                    slot_of.insert(l.tops.first().cloned().unwrap_or_default(), top);
                    layers.push(exec);
                }
                _ => {} // Input/MemoryData/Loss/Dropout 等跳过
            }
        }

        if layers.is_empty() {
            return Err("网络无可执行层".into());
        }
        if scale_num == 0 || scale_den == 0 || scale_num % scale_den != 0 {
            return Err(format!("inner_scale 异常: {}/{}", scale_num, scale_den));
        }
        let inner_scale = scale_num / scale_den;
        let output_slot = layers.last().unwrap().top();
        let (ow, _oh, _oc) = shapes[output_slot];
        if ow % inner_scale != 0 {
            return Err(format!("输出宽度 {ow} 不被 inner_scale {inner_scale} 整除"));
        }
        let core = ow / inner_scale;
        if PROBE < core || (PROBE - core) % 2 != 0 {
            return Err(format!("net_offset 异常: probe={PROBE} core={core}"));
        }
        let probe_offset = (PROBE - core) / 2;

        Ok(WaifuModel {
            layers,
            in_ch,
            net_offset: probe_offset,
            inner_scale,
            kind,
            n_slots: shapes.len(),
            output_slot,
        })
    }

    /// 各槽位形状（给定输入尺寸；GPU 会话纹理分配用）
    pub fn slot_shapes(
        &self,
        in_w: usize,
        in_h: usize,
    ) -> Result<Vec<(usize, usize, usize)>, String> {
        let mut sh = vec![(in_w, in_h, self.in_ch)];
        for l in &self.layers {
            let s = layer_out_shape(l, &sh)?;
            sh.push(s);
        }
        Ok(sh)
    }

    /// 从 info.json 读取原版 offset（覆盖 shape_probe 的推算值）
    ///
    /// offset = 每边 pad 量（同时是输出中心裁剪起点）。
    /// noise 模型可用 offset_noise（cunet），scale 模型用 offset。
    pub fn with_info_offset(mut self, model_root: &std::path::Path, noise: bool) -> Self {
        let info = model_root.join(self.kind.dir_name()).join("info.json");
        if let Ok(txt) = std::fs::read_to_string(&info) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                let key = if noise && v.get("offset_noise").is_some() {
                    "offset_noise"
                } else {
                    "offset"
                };
                if let Some(off) = v.get(key).and_then(|x| x.as_u64()) {
                    log::info!(
                        "[upscale] {} {} offset={}（info.json，probe={})",
                        self.kind.dir_name(),
                        if noise { "noise" } else { "scale" },
                        off,
                        self.net_offset
                    );
                    self.net_offset = off as usize;
                }
            }
        }
        self
    }

    fn make_conv(l: &LayerDef, leaky_slope: f32) -> Result<ExecLayer, String> {
        let p: ConvParam = l.conv.ok_or("Convolution 无参数")?;
        if l.blobs.is_empty() {
            return Err(format!("层 {} 无权重", l.name));
        }
        let w = &l.blobs[0];
        if w.shape.len() != 4 {
            return Err(format!("层 {} 权重非 4D: {:?}", l.name, w.shape));
        }
        let bias = l
            .blobs
            .get(1)
            .map(|b| b.data.clone())
            .unwrap_or_else(|| vec![0.0; w.shape[0]]);
        Ok(ExecLayer::Conv {
            weight: w.data.clone(),
            bias,
            in_ch: w.shape[1],
            out_ch: w.shape[0],
            kernel: p.kernel_size as usize,
            pad: p.pad as usize,
            stride: p.stride.max(1) as usize,
            dilation: p.dilation.max(1) as usize,
            leaky_slope,
            bottom: 0,
            top: 0,
        })
    }

    fn make_deconv(l: &LayerDef, leaky_slope: f32) -> Result<ExecLayer, String> {
        let p: ConvParam = l.conv.ok_or("Deconvolution 无参数")?;
        let w = l.blobs.first().ok_or("Deconv 无权重")?;
        if w.shape.len() != 4 {
            return Err(format!("deconv {} 权重非 4D: {:?}", l.name, w.shape));
        }
        let bias = l
            .blobs
            .get(1)
            .map(|b| b.data.clone())
            .unwrap_or_else(|| vec![0.0; w.shape[1]]);
        // Caffe deconv 权重布局 [in_ch, out_ch, kh, kw]（正卷积布局）
        Ok(ExecLayer::Deconv {
            weight: w.data.clone(),
            bias,
            in_ch: w.shape[0],
            out_ch: w.shape[1],
            kernel: p.kernel_size as usize,
            stride: p.stride.max(1) as usize,
            pad: p.pad as usize,
            leaky_slope,
            bottom: 0,
            top: 0,
        })
    }

    /// 模型目录 + 文件名加载（net_offset 取 info.json 原版值）
    pub fn load(kind: ModelKind, model_root: &std::path::Path, file: &str) -> Result<Self, String> {
        if kind.is_realesrgan() {
            return Self::load_realesrgan(kind, model_root);
        }
        let is_noise = file.starts_with("noise");
        let path = model_root.join(kind.dir_name()).join(file);
        let net = super::caffemodel::load_caffemodel(&path)?;
        Ok(Self::build(kind, &net)?.with_info_offset(model_root, is_noise))
    }

    /// Real-ESRGAN .pth 加载（RRDBNet state_dict → ExecLayer 拓扑）
    fn load_realesrgan(kind: ModelKind, model_root: &std::path::Path) -> Result<Self, String> {
        let file = kind.scale_file();
        // 两处探测：root/<file>（平铺布局）与 root/RealESRGAN/<file>（子目录布局）
        let p1 = model_root.join(file);
        let p2 = model_root.join(kind.dir_name()).join(file);
        let path = if p1.exists() {
            p1
        } else if p2.exists() {
            p2
        } else {
            return Err(format!(
                "Real-ESRGAN 模型缺失: {}（请下载 {} 放到 models 目录）",
                p1.display(),
                file
            ));
        };
        let num_block = match kind {
            ModelKind::RealEsrganX4 => 23,
            ModelKind::RealEsrganAnime6B => 6,
            _ => unreachable!(),
        };
        let mut sd = super::pth::PthFile::open(&path)?;
        Self::build_rrdbnet(kind, &mut sd, num_block)
    }
}

// ==================== Real-ESRGAN RRDBNet 构建 ====================

/// RRDBNet 拓扑构建器（BasicSR rrdbnet_arch.py 等价）：
///
/// ```text
/// conv_first(3→64)                                   无激活
/// ×num_block { RRDB = RDB→RDB→RDB }                  每块输出 ≡ 输入通道
/// conv_body(64→64)                                   无激活
/// feat = feat + conv_body(body)                      主干残差
/// ×2 { nearest-up2 → conv_up(64→64) + lrelu 0.2 }    上采样 ×4
/// conv_hr(64→64) + lrelu → conv_last(64→3)           输出头（无激活）
/// ```
/// RDB（ResidualDenseBlock，gc=32）：
/// ```text
/// c1 = lrelu(conv1(x))                        64→32
/// c2 = lrelu(conv2(cat(x, c1)))               96→32
/// c3 = lrelu(conv3(cat(x, c1, c2)))           128→32
/// c4 = lrelu(conv4(cat(x, c1, c2, c3)))       160→32
/// c5 = conv5(cat(x, c1..c4))                  192→64（无激活）
/// out = 0.2·c5 + x                            res_scale=0.2
/// ```
struct RrdbBuilder {
    layers: Vec<ExecLayer>,
    shapes: Vec<(usize, usize, usize)>, // (w, h, c)
}

impl RrdbBuilder {
    fn new(in_ch: usize) -> Self {
        RrdbBuilder {
            layers: Vec::new(),
            shapes: vec![(0, 0, in_ch)], // 槽 0 = 输入（尺寸运行时定）
        }
    }

    fn slot(&mut self, shape: (usize, usize, usize)) -> usize {
        self.shapes.push(shape);
        self.shapes.len() - 1
    }

    /// conv3×3 s1 p1 + leaky（leaky=1.0 即无激活）；权重 [out,in,3,3] 与 PyTorch 同布局
    fn conv(
        &mut self,
        sd: &mut super::pth::PthFile,
        name: &str,
        bottom: usize,
        leaky: f32,
    ) -> Result<usize, String> {
        let wname = format!("{}.weight", name);
        let bname = format!("{}.bias", name);
        let meta = sd
            .tensor(&wname)
            .ok_or_else(|| format!("pth 缺少 {}", wname))?;
        if meta.shape.len() != 4 {
            return Err(format!("{} 权重非 4D: {:?}", wname, meta.shape));
        }
        let (out_ch, in_ch, kh, kw) = (meta.shape[0], meta.shape[1], meta.shape[2], meta.shape[3]);
        if kh != kw {
            return Err(format!("{} 非方形卷积核 {}x{}", name, kh, kw));
        }
        if kh != 3 {
            return Err(format!("{} 非期望的 3x3 核（{}）", name, kh));
        }
        let (bw, bh, bc) = self.shapes[bottom];
        if bc != in_ch {
            return Err(format!(
                "{} 输入通道不符: 权重 {} vs bottom {}",
                name, in_ch, bc
            ));
        }
        let weight = sd.get_f32(&wname)?;
        let bias = sd.get_f32(&bname)?;
        // k3 p1 s1 → 输出空间 = 输入空间
        let top = self.slot((bw, bh, out_ch));
        self.layers.push(ExecLayer::Conv {
            weight,
            bias,
            in_ch,
            out_ch,
            kernel: 3,
            pad: 1,
            stride: 1,
            dilation: 1,
            leaky_slope: leaky,
            bottom,
            top,
        });
        Ok(top)
    }

    /// 通道拼接（>3 输入自动拆分为 ≤3 输入链——GPU SRV 槽限制）
    fn concat(&mut self, bottoms: &[usize]) -> usize {
        if bottoms.len() == 1 {
            return bottoms[0];
        }
        let mut cur: Vec<usize> = bottoms.to_vec();
        while cur.len() > 3 {
            let head: Vec<usize> = cur.drain(..3).collect();
            let t = self.concat_one(&head);
            cur.insert(0, t);
        }
        self.concat_one(&cur)
    }

    fn concat_one(&mut self, bottoms: &[usize]) -> usize {
        let (w, h, _) = self.shapes[bottoms[0]];
        let c: usize = bottoms.iter().map(|b| self.shapes[*b].2).sum();
        let top = self.slot((w, h, c));
        self.layers.push(ExecLayer::Concat {
            bottoms: bottoms.to_vec(),
            top,
        });
        top
    }

    fn up2(&mut self, bottom: usize) -> usize {
        let (w, h, c) = self.shapes[bottom];
        let top = self.slot((w * 2, h * 2, c));
        self.layers.push(ExecLayer::NearestUp2 { bottom, top });
        top
    }

    fn add(&mut self, a: usize, b: usize) -> usize {
        let shape = self.shapes[a];
        let top = self.slot(shape);
        self.layers.push(ExecLayer::EltwiseAdd { a, b, top });
        top
    }

    /// out = β·a + b（RRDB 外层残差缩放）
    fn scaled_add(&mut self, a: usize, b: usize, beta: f32) -> usize {
        let shape = self.shapes[a];
        let top = self.slot(shape);
        self.layers.push(ExecLayer::ScaledAdd { a, b, beta, top });
        top
    }

    /// RDB 块（conv1..conv5 + 密集连接 concat + 0.2 残差）
    fn rdb(
        &mut self,
        sd: &mut super::pth::PthFile,
        prefix: &str,
        x: usize,
    ) -> Result<usize, String> {
        let c1 = self.conv(sd, &format!("{}.conv1", prefix), x, 0.2)?;
        let cat1 = self.concat(&[x, c1]);
        let c2 = self.conv(sd, &format!("{}.conv2", prefix), cat1, 0.2)?;
        let cat2 = self.concat(&[x, c1, c2]);
        let c3 = self.conv(sd, &format!("{}.conv3", prefix), cat2, 0.2)?;
        let cat3 = self.concat(&[x, c1, c2, c3]);
        let c4 = self.conv(sd, &format!("{}.conv4", prefix), cat3, 0.2)?;
        let cat4 = self.concat(&[x, c1, c2, c3, c4]);
        let c5 = self.conv(sd, &format!("{}.conv5", prefix), cat4, 1.0)?;
        // out = 0.2·c5 + x（res_scale）
        let shape = self.shapes[x];
        let top = self.slot(shape);
        self.layers.push(ExecLayer::ScaledAdd {
            a: c5,
            b: x,
            beta: 0.2,
            top,
        });
        Ok(top)
    }
}

impl WaifuModel {
    fn build_rrdbnet(
        kind: ModelKind,
        sd: &mut super::pth::PthFile,
        num_block: usize,
    ) -> Result<Self, String> {
        let mut b = RrdbBuilder::new(3);

        let feat = b.conv(sd, "conv_first", 0, 1.0)?; // 无激活
        let mut body = feat;
        for i in 0..num_block {
            let x = body;
            let mut r = x;
            for j in 1..=3 {
                r = b.rdb(sd, &format!("body.{}.rdb{}", i, j), r)?;
            }
            // RRDB 残差（残差中残差）：out = 0.2·rdb3 输出 + 块输入
            body = b.scaled_add(r, x, 0.2);
        }
        let cb = b.conv(sd, "conv_body", body, 1.0)?; // 无激活
        let main = b.add(feat, cb); // 主干残差

        // 上采样 ×4：nearest-up2 → conv_up（两次）
        let u1 = b.up2(main);
        let c_up1 = b.conv(sd, "conv_up1", u1, 0.2)?;
        let u2 = b.up2(c_up1);
        let c_up2 = b.conv(sd, "conv_up2", u2, 0.2)?;
        // 输出头
        let c_hr = b.conv(sd, "conv_hr", c_up2, 0.2)?;
        let out = b.conv(sd, "conv_last", c_hr, 1.0)?; // 无激活

        Ok(WaifuModel {
            layers: b.layers,
            in_ch: 3,
            net_offset: 0,
            inner_scale: 4,
            kind,
            n_slots: b.shapes.len(),
            output_slot: out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_root() -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap_or_default();
        exe.parent()
            .map(|p| p.join("models"))
            .filter(|p| p.exists())
            .unwrap_or_else(|| std::path::PathBuf::from(r"e:\jietu\图片放大器\models"))
    }

    /// 全模型族加载 + 拓扑指标（P5 验证：upresnet10/cunet 可构建）
    #[test]
    fn test_load_all_models() {
        let root = model_root();
        let cases = [
            (
                ModelKind::Upconv7AnimeStyleArtRgb,
                "scale2.0x_model.json.caffemodel",
                2,
                14,
            ),
            (
                ModelKind::Upconv7Photo,
                "scale2.0x_model.json.caffemodel",
                2,
                14,
            ),
            (
                ModelKind::Upresnet10,
                "scale2.0x_model.json.caffemodel",
                2,
                26,
            ),
            (ModelKind::Cunet, "scale2.0x_model.json.caffemodel", 2, 36),
            (
                ModelKind::Upresnet10,
                "noise1_scale2.0x_model.json.caffemodel",
                2,
                26,
            ),
            (
                ModelKind::Cunet,
                "noise1_scale2.0x_model.json.caffemodel",
                2,
                28,
            ),
            // noise-only 模型仅 cunet 家族携带（upresnet10/upconv_7 无）
            (ModelKind::Cunet, "noise1_model.json.caffemodel", 1, 28),
        ];
        for (kind, file, want_scale, want_offset) in cases {
            let m = WaifuModel::load(kind, &root, file)
                .unwrap_or_else(|e| panic!("{} {} 加载失败: {}", kind.dir_name(), file, e));
            assert_eq!(
                m.inner_scale,
                want_scale,
                "{} {} inner_scale",
                kind.dir_name(),
                file
            );
            assert_eq!(
                m.net_offset,
                want_offset,
                "{} {} net_offset",
                kind.dir_name(),
                file
            );
            eprintln!(
                "[test] {} {}: {} 层 / {} 槽位 / scale {} / offset {}",
                kind.dir_name(),
                file,
                m.layers.len(),
                m.n_slots,
                m.inner_scale,
                m.net_offset
            );
        }
    }

    /// Real-ESRGAN 家族：pth 解析 + RRDBNet 拓扑构建 + 形状链
    #[test]
    fn test_load_realesrgan() {
        let root = model_root();
        for kind in [ModelKind::RealEsrganX4, ModelKind::RealEsrganAnime6B] {
            let f = kind.scale_file();
            let exists = root.join(&f).exists() || root.join("RealESRGAN").join(&f).exists();
            if !exists {
                eprintln!("[test] {} 缺失，跳过", f);
                continue;
            }
            let m = WaifuModel::load(kind, &root, kind.scale_file())
                .unwrap_or_else(|e| panic!("{} 加载失败: {}", f, e));
            assert_eq!(m.in_ch, 3, "{} in_ch", f);
            assert_eq!(m.inner_scale, 4, "{} inner_scale", f);
            assert_eq!(m.net_offset, 0, "{} net_offset", f);
            // 形状链：48×48 → ×4 → 192×192（conv 全 p1s1 尺寸不变 + 两次 up2）
            let shapes = m.slot_shapes(48, 48).unwrap();
            assert_eq!(shapes[m.output_slot], (192, 192, 3), "{} 输出形状", f);
            eprintln!(
                "[test] {}：{} 层 / {} 槽位 / ×4",
                f,
                m.layers.len(),
                m.n_slots
            );
        }
    }

    /// Real-ESRGAN CPU 前向：黑图空间平坦（bias 链）+ 灰图有限值 + 尺寸
    #[test]
    fn test_realesrgan_cpu_forward() {
        let root = model_root();
        for kind in [ModelKind::RealEsrganX4, ModelKind::RealEsrganAnime6B] {
            let f = kind.scale_file();
            let exists = root.join(&f).exists() || root.join("RealESRGAN").join(&f).exists();
            if !exists {
                eprintln!("[test] {} 缺失，跳过", f);
                continue;
            }
            let m = WaifuModel::load(kind, &root, kind.scale_file()).unwrap();
            // 16×16 黑图 → 64×64；常数输入下中心区数学平坦
            // （conv zero-pad 边界圈除外——PyTorch 语义，边界 1px 圈有边界效应）
            let inp = super::super::cpu::Tensor::new(3, 16, 16);
            let out = super::super::cpu::forward(&m, &inp).unwrap();
            assert_eq!((out.w, out.h, out.c), (64, 64, 3), "{} 尺寸", f);
            let b = 8; // 输出边界圈（输入 1px 边界 × 深网感受野扩散）
            for c in 0..3 {
                let (mut mn, mut mx) = (f32::INFINITY, f32::NEG_INFINITY);
                for y in b..64 - b {
                    for x in b..64 - b {
                        let v = out.at(c, y, x);
                        mn = mn.min(v);
                        mx = mx.max(v);
                    }
                }
                // 16×16 小图 + 23 块深网：zero-pad 边界效应经感受野扩散到中心，
                // 残余 ~0.005 量级属正常（黄金对拍 51dB 已证实现正确）
                assert!(
                    (mx - mn).abs() < 0.02,
                    "{} 黑图 ch{c} 中心区非平坦: {mn}..{mx}",
                    f
                );
            }
            eprintln!("[test] {} 黑图输出 ch0={:.4}", f, out.at(0, 32, 32));

            // 灰图 0.5 → 有限值（网络深度大，NaN/Inf 即数值问题）
            let mut g = super::super::cpu::Tensor::new(3, 16, 16);
            for v in g.data.iter_mut() {
                *v = 0.5;
            }
            let out2 = super::super::cpu::forward(&m, &g).unwrap();
            let all_finite = out2.data.iter().all(|v| v.is_finite());
            assert!(all_finite, "{} 灰图输出含非有限值", f);
            let (mn, mx) = out2
                .data
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), v| {
                    (a.min(*v), b.max(*v))
                });
            eprintln!("[test] {} 灰图输出值域 {mn:.4}..{mx:.4}", f);
        }
    }

    /// 隔离调试：对比 Rust 解析的 blob 形状与已知值（upresnet10 scale 模型）
    #[test]
    fn test_parsed_blob_shapes() {
        let root = model_root();
        let net = super::super::caffemodel::load_caffemodel(
            &root
                .join("upresnet10")
                .join("scale2.0x_model.json.caffemodel"),
        )
        .unwrap();
        for (i, l) in net.layers.iter().enumerate().take(14) {
            let blobs: Vec<String> = l
                .blobs
                .iter()
                .map(|b| {
                    format!(
                        "[{}]×{}",
                        b.shape
                            .iter()
                            .map(|d| d.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                        b.data.len()
                    )
                })
                .collect();
            eprintln!(
                "[blob] {i:>2} {} {} ({}) blobs: {}",
                l.layer_type,
                l.name,
                l.blobs.len(),
                blobs.join(" ")
            );
        }
    }

    /// 隔离调试：upconv7 纯色输入 → 中心区平坦性
    ///
    /// conv 对常数输入数学上严格平坦；deconv（k4s2p3）边缘 pad 丢弃不均
    /// + 训练权重奇偶 parity 残差 → 全图存在 ~0.002 的固有微小空间差异
    /// （原版 CUI 同样如此，黄金对拍已证位级一致）。故只检中心区、阈值放宽。
    #[test]
    fn test_flat_input_flatness() {
        let root = model_root();
        let m = WaifuModel::load(
            ModelKind::Upconv7AnimeStyleArtRgb,
            &root,
            "scale2.0x_model.json.caffemodel",
        )
        .unwrap();
        let mut inp = super::super::cpu::Tensor::new(3, 156, 156);
        for i in inp.data.iter_mut() {
            *i = 0.501961;
        }
        let out = super::super::cpu::forward(&m, &inp).unwrap();
        // 中心区（去 32px 边界）
        let b = 32.min(out.h / 4).min(out.w / 4);
        let (mut mn, mut mx) = (f32::INFINITY, f32::NEG_INFINITY);
        for c in 0..out.c {
            for y in b..out.h - b {
                for x in b..out.w - b {
                    let v = out.at(c, y, x);
                    mn = mn.min(v);
                    mx = mx.max(v);
                }
            }
        }
        eprintln!(
            "[flat] upconv7 输出: {}x{}x{} 中心区值域 {mn:.6}..{mx:.6}",
            out.c, out.h, out.w
        );
        assert!((mx - mn).abs() < 0.01, "纯色输入中心区非平坦: {mn}..{mx}");
    }

    /// CPU 前向（含 SE 算子）：输出 = 未裁剪的网络原始输出（与 slot_shapes 一致）
    #[test]
    fn test_cpu_forward_se_models() {
        let root = model_root();
        for (kind, file) in [
            (ModelKind::Upresnet10, "scale2.0x_model.json.caffemodel"),
            (ModelKind::Cunet, "scale2.0x_model.json.caffemodel"),
            (
                ModelKind::Upconv7AnimeStyleArtRgb,
                "scale2.0x_model.json.caffemodel",
            ),
        ] {
            let m = WaifuModel::load(kind, &root, file).unwrap();
            // 64×64 输入 → pad offset → 前向
            let inp = super::super::cpu::Tensor::new(3, 64, 64);
            let padded = super::super::pipeline::pad_replicate(&inp, m.net_offset);
            let out = super::super::cpu::forward(&m, &padded)
                .unwrap_or_else(|e| panic!("{} 前向失败: {}", kind.dir_name(), e));
            let want = m.slot_shapes(padded.w, padded.h).unwrap()[m.output_slot];
            assert_eq!((out.w, out.h, out.c), want, "{} 输出尺寸", kind.dir_name());
            eprintln!(
                "[test] {} 前向 OK: {}x{}x{}",
                kind.dir_name(),
                out.w,
                out.h,
                out.c
            );
        }
    }
}

/// 层的 stride（inner_scale 推算用：deconv 乘 / conv 除）
fn exec_stride(l: &ExecLayer) -> usize {
    match l {
        ExecLayer::Conv { stride, .. } => *stride,
        ExecLayer::Deconv { stride, .. } => *stride,
        _ => 1,
    }
}

/// 回填 top 槽位（构建期占位 0 → 实际槽号）
fn set_top(mut l: ExecLayer, top: usize) -> ExecLayer {
    match &mut l {
        ExecLayer::Conv { top: t, .. }
        | ExecLayer::Deconv { top: t, .. }
        | ExecLayer::GlobalAvgPool { top: t, .. }
        | ExecLayer::Sigmoid { top: t, .. }
        | ExecLayer::Scale { top: t, .. }
        | ExecLayer::Axpy { top: t, .. }
        | ExecLayer::CropCenter { top: t, .. }
        | ExecLayer::Crop { top: t, .. }
        | ExecLayer::EltwiseAdd { top: t, .. }
        | ExecLayer::Concat { top: t, .. }
        | ExecLayer::NearestUp2 { top: t, .. }
        | ExecLayer::ScaledAdd { top: t, .. } => *t = top,
    }
    l
}

/// 回填 bottom + top 槽位（Conv/Deconv：构建期占位 0 → 实际槽号）
fn set_slots(mut l: ExecLayer, bottom: usize, top: usize) -> ExecLayer {
    match &mut l {
        ExecLayer::Conv {
            bottom: b, top: t, ..
        }
        | ExecLayer::Deconv {
            bottom: b, top: t, ..
        } => {
            *b = bottom;
            *t = top;
        }
        _ => {}
    }
    l
}
