//! CPU 推理引擎：conv（任意 stride）/ deconv / SE 算子 / eltwise（CHW float32）
//!
//! 数值对齐原版 Caffe：
//! - 输入归一化 0-1（对应 stImage::ConvertToFloat 的 1/255）
//! - 直接卷积（非 Winograd），浮点累加顺序与 Caffe CPU 一致
//! - 输出端 clip 到 [0,1] 再 ×255（对应 Postprocess）
//!
//! 执行模型（P5）：blob 槽位竞技场——每层按 bottom 槽位取输入，
//! 输出写 top 槽位；blob 最后一次消费后即释放（内存控制）。

use super::model::ExecLayer;

/// 特征图（CHW，连续布局）
#[derive(Clone)]
pub struct Tensor {
    pub c: usize,
    pub h: usize,
    pub w: usize,
    pub data: Vec<f32>,
}

impl Tensor {
    pub fn new(c: usize, h: usize, w: usize) -> Self {
        Tensor {
            c,
            h,
            w,
            data: vec![0.0; c * h * w],
        }
    }
    #[inline]
    pub fn at(&self, ch: usize, y: usize, x: usize) -> f32 {
        self.data[(ch * self.h + y) * self.w + x]
    }
    #[inline]
    pub fn set(&mut self, ch: usize, y: usize, x: usize, v: f32) {
        let i = (ch * self.h + y) * self.w + x;
        self.data[i] = v;
    }
}

/// 任意 kernel/stride 直接卷积 + bias + leaky ReLU，行级并行
///
/// 输出尺寸 = floor((in + 2p - d(k-1) - 1)/s) + 1
#[allow(clippy::too_many_arguments)]
fn conv2d(
    input: &Tensor,
    weight: &[f32], // [out_ch, in_ch, kh, kw]
    bias: &[f32],   // [out_ch]
    out_ch: usize,
    kernel: usize,
    pad: usize,
    stride: usize,
    dilation: usize,
    leaky_slope: f32,
) -> Tensor {
    let (in_ch, ih, iw) = (input.c, input.h, input.w);
    let oh = (ih + 2 * pad).saturating_sub((kernel - 1) * dilation + 1) / stride + 1;
    let ow = (iw + 2 * pad).saturating_sub((kernel - 1) * dilation + 1) / stride + 1;
    debug_assert!(oh > 0 && ow > 0);
    let mut out = Tensor::new(out_ch, oh, ow);

    // 边界 zero-pad 采样（PyTorch Conv2d 默认语义；与 GPU shader 的越界跳过一致）。
    // waifu2x 家族输入已 replicate-pad offset（≥7 > conv pad），边界从不越界——
    // 该改动对其无影响；Real-ESRGAN（net_offset=0）依赖本语义。
    let sample = |ch: usize, y: isize, x: isize| -> f32 {
        if y < 0 || y >= ih as isize || x < 0 || x >= iw as isize {
            return 0.0;
        }
        input.at(ch, y as usize, x as usize)
    };

    // 行级并行：CHW 布局下"平面行" = 某通道某 y 行（ow 个连续 float）
    let plane_rows = out_ch * oh;
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8);
    if nthreads <= 1 || plane_rows < 16 {
        conv_rows_single(
            &mut out.data,
            0..plane_rows,
            oh,
            ow,
            input,
            weight,
            bias,
            kernel,
            pad,
            stride,
            dilation,
            leaky_slope,
            &sample,
        );
    } else {
        let chunk = (plane_rows + nthreads - 1) / nthreads;
        let mut slices: Vec<(usize, &mut [f32])> = Vec::new();
        let mut rest: &mut [f32] = &mut out.data;
        let mut start = 0usize;
        while start < plane_rows {
            let end = (start + chunk).min(plane_rows);
            let len = (end - start) * ow;
            let (a, b) = rest.split_at_mut(len);
            slices.push((start, a));
            rest = b;
            start = end;
        }
        std::thread::scope(|scope| {
            for (base, slice) in slices.iter_mut() {
                let sample_ref = &sample;
                let s: &mut [f32] = std::mem::replace(slice, &mut []);
                let base = *base;
                scope.spawn(move || {
                    conv_rows_single(
                        s,
                        base..base + s.len() / ow,
                        oh,
                        ow,
                        input,
                        weight,
                        bias,
                        kernel,
                        pad,
                        stride,
                        dilation,
                        leaky_slope,
                        sample_ref,
                    );
                });
            }
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// conv 多线程 vs 单线程一致性（随机输入 64→64）
    #[test]
    fn test_conv_thread_consistency() {
        // 确定性伪随机输入
        let (c, h, w) = (64usize, 128usize, 128usize);
        let mut input = Tensor::new(c, h, w);
        let mut seed = 12345u64;
        for v in input.data.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *v = ((seed >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0;
        }
        let (out_ch, k) = (64usize, 3usize);
        let mut weight = vec![0.0f32; out_ch * c * k * k];
        let mut seed = 999u64;
        for v in weight.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *v = ((seed >> 33) as f32 / u32::MAX as f32) * 0.2 - 0.1;
        }
        let bias: Vec<f32> = (0..out_ch).map(|i| i as f32 * 0.001).collect();

        // 单线程（强制：nthreads=1 分支在 plane_rows>=16 时不走——直接调用 conv_rows_single）
        let threaded = conv2d(&input, &weight, &bias, out_ch, k, 0, 1, 1, 0.1);
        // 手动单线程
        let oh = h - 2;
        let ow = w - 2;
        let mut single = Tensor::new(out_ch, oh, ow);
        let sample = |ch: usize, y: isize, x: isize| -> f32 {
            let yy = y.clamp(0, h as isize - 1) as usize;
            let xx = x.clamp(0, w as isize - 1) as usize;
            input.at(ch, yy, xx)
        };
        conv_rows_single(
            &mut single.data,
            0..out_ch * oh,
            oh,
            ow,
            &input,
            &weight,
            &bias,
            k,
            0,
            1,
            1,
            0.1,
            &sample,
        );
        assert_eq!(threaded.data.len(), single.data.len());
        let mut max_err = 0.0f32;
        for (a, b) in threaded.data.iter().zip(single.data.iter()) {
            max_err = max_err.max((a - b).abs());
        }
        eprintln!("[conv-thread] max_err={max_err:.2e}");
        assert!(max_err < 1e-6, "conv 多线程不一致: {max_err}");
    }
}

/// 计算一组平面行（CHW：第 n 行 = 通道 n/oh 的第 n%oh 行）
#[allow(clippy::too_many_arguments)]
fn conv_rows_single(
    data: &mut [f32],
    rows: std::ops::Range<usize>,
    oh: usize,
    ow: usize,
    input: &Tensor,
    weight: &[f32],
    bias: &[f32],
    kernel: usize,
    pad: usize,
    stride: usize,
    dilation: usize,
    leaky_slope: f32,
    sample: &dyn Fn(usize, isize, isize) -> f32,
) {
    let in_ch = input.c;
    for (li, n) in rows.clone().enumerate() {
        let oc = n / oh;
        let y = n % oh;
        let row = &mut data[li * ow..(li + 1) * ow];
        for x in 0..ow {
            let w_base = oc * in_ch * kernel * kernel;
            let mut acc = bias[oc];
            for ic in 0..in_ch {
                let w_ch = w_base + ic * kernel * kernel;
                for ky in 0..kernel {
                    let sy = (y * stride + ky * dilation) as isize - pad as isize;
                    for kx in 0..kernel {
                        let sx = (x * stride + kx * dilation) as isize - pad as isize;
                        acc += weight[w_ch + ky * kernel + kx] * sample(ic, sy, sx);
                    }
                }
            }
            let v = if acc >= 0.0 { acc } else { acc * leaky_slope };
            row[x] = v;
        }
    }
}

/// 转置卷积（deconv）：out = (in-1)*stride - 2*pad + kernel
///
/// Caffe Deconvolution 权重布局 [in_ch, out_ch, kh, kw]，
/// 数学：out[sy, sx, oc] += w[ic, oc, ky, kx] * in[ic, y, x]
/// 其中 sy = y*stride + ky - pad
#[allow(clippy::too_many_arguments)]
fn deconv2d(
    input: &Tensor,
    weight: &[f32], // [in_ch, out_ch, kh, kw]
    bias: &[f32],   // [out_ch]
    out_ch: usize,
    kernel: usize,
    stride: usize,
    pad: usize,
    leaky_slope: f32,
) -> Tensor {
    let (in_ch, ih, iw) = (input.c, input.h, input.w);
    let oh = (ih - 1) * stride + kernel - 2 * pad;
    let ow = (iw - 1) * stride + kernel - 2 * pad;
    debug_assert!(oh > 0 && ow > 0);
    let mut out = Tensor::new(out_ch, oh, ow);

    // scatter 累加（按输入像素串行；deconv 层输入尺寸小，占整网耗时比例低）
    for ic in 0..in_ch {
        for y in 0..ih {
            for x in 0..iw {
                let iv = input.at(ic, y, x);
                for oc in 0..out_ch {
                    let w_base = (ic * out_ch + oc) * kernel * kernel;
                    for ky in 0..kernel {
                        let sy = y * stride + ky;
                        if sy < pad {
                            continue;
                        }
                        let sy = sy - pad;
                        if sy >= oh {
                            continue;
                        }
                        for kx in 0..kernel {
                            let sx = x * stride + kx;
                            if sx < pad {
                                continue;
                            }
                            let sx = sx - pad;
                            if sx >= ow {
                                continue;
                            }
                            let wv = weight[w_base + ky * kernel + kx];
                            let idx = (oc * oh + sy) * ow + sx;
                            out.data[idx] += wv * iv;
                        }
                    }
                }
            }
        }
    }
    // bias + leaky
    for oc in 0..out_ch {
        let b = bias[oc];
        for i in 0..oh * ow {
            let v = out.data[oc * oh * ow + i] + b;
            out.data[oc * oh * ow + i] = if v >= 0.0 { v } else { v * leaky_slope };
        }
    }
    out
}

/// 全局平均池化（SE squeeze）：C×H×W → C×1×1
fn global_avg_pool(t: &Tensor) -> Tensor {
    let mut out = Tensor::new(t.c, 1, 1);
    let n = (t.h * t.w) as f32;
    for c in 0..t.c {
        let base = c * t.h * t.w;
        let mut acc = 0.0f32;
        for i in 0..t.h * t.w {
            acc += t.data[base + i];
        }
        out.data[c] = acc / n;
    }
    out
}

/// 逐元素 sigmoid
fn sigmoid(t: &Tensor) -> Tensor {
    let mut out = Tensor::new(t.c, t.h, t.w);
    for (o, v) in out.data.iter_mut().zip(t.data.iter()) {
        *o = 1.0 / (1.0 + (-*v).exp());
    }
    out
}

/// 通道缩放（SE gate）：data × vec[c]
fn scale_ch(data: &Tensor, vec: &Tensor) -> Tensor {
    debug_assert_eq!(vec.c, data.c);
    let mut out = Tensor::new(data.c, data.h, data.w);
    for c in 0..data.c {
        let s = vec.data[c];
        let base = c * data.h * data.w;
        for i in 0..data.h * data.w {
            out.data[base + i] = data.data[base + i] * s;
        }
    }
    out
}

/// out = alpha[c]·x + y（alpha 为 C×1×1 广播）
fn axpy(alpha: &Tensor, x: &Tensor, y: &Tensor) -> Tensor {
    debug_assert_eq!((x.c, x.h, x.w), (y.c, y.h, y.w));
    let mut out = Tensor::new(x.c, x.h, x.w);
    for c in 0..x.c {
        let a = alpha.data[c];
        let base = c * x.h * x.w;
        for i in 0..x.h * x.w {
            out.data[base + i] = a * x.data[base + i] + y.data[base + i];
        }
    }
    out
}

/// 中心裁剪：每边去 crop_h/crop_w 像素
fn crop_center(t: &Tensor, crop_h: usize, crop_w: usize) -> Tensor {
    let (oh, ow) = (t.h - 2 * crop_h, t.w - 2 * crop_w);
    let mut out = Tensor::new(t.c, oh, ow);
    for c in 0..t.c {
        for y in 0..oh {
            let src = (c * t.h + y + crop_h) * t.w + crop_w;
            let dst = (c * oh + y) * ow;
            out.data[dst..dst + ow].copy_from_slice(&t.data[src..src + ow]);
        }
    }
    out
}

/// 参考锚定裁剪：out[y,x] = in[y+off, x+off]，输出尺寸 = reference 尺寸
fn crop_ref(t: &Tensor, reference: &Tensor, offset: usize) -> Tensor {
    let (oh, ow) = (reference.h, reference.w);
    debug_assert!(t.h >= offset + oh && t.w >= offset + ow);
    let mut out = Tensor::new(t.c, oh, ow);
    for c in 0..t.c {
        for y in 0..oh {
            let src = (c * t.h + y + offset) * t.w + offset;
            let dst = (c * oh + y) * ow;
            out.data[dst..dst + ow].copy_from_slice(&t.data[src..src + ow]);
        }
    }
    out
}

/// 逐元素相加（残差）
fn eltwise_add(a: &Tensor, b: &Tensor) -> Tensor {
    debug_assert_eq!((a.c, a.h, a.w), (b.c, b.h, b.w));
    let mut out = Tensor::new(a.c, a.h, a.w);
    for i in 0..out.data.len() {
        out.data[i] = a.data[i] + b.data[i];
    }
    out
}

/// 通道维拼接（RRDB 密集连接）：输出通道序 = 输入按序拼接
fn concat_ch(inputs: &[&Tensor]) -> Result<Tensor, String> {
    let first = inputs.first().ok_or("concat 无输入")?;
    let (h, w) = (first.h, first.w);
    for t in inputs {
        if (t.h, t.w) != (h, w) {
            return Err(format!("空间尺寸不一致 {}x{} vs {}x{}", w, h, t.w, t.h));
        }
    }
    let c_total: usize = inputs.iter().map(|t| t.c).sum();
    let mut out = Tensor::new(c_total, h, w);
    let mut dst_c = 0usize;
    for t in inputs {
        let n = t.c * t.h * t.w;
        out.data[dst_c * h * w..dst_c * h * w + n].copy_from_slice(&t.data[..n]);
        dst_c += t.c;
    }
    Ok(out)
}

/// 最近邻 ×2 上采样（RRDBNet：F.interpolate(nearest)）
fn nearest_up2(t: &Tensor) -> Tensor {
    let (c, h, w) = (t.c, t.h, t.w);
    let (oh, ow) = (h * 2, w * 2);
    let mut out = Tensor::new(c, oh, ow);
    for ch in 0..c {
        let src = ch * h * w;
        let dst = ch * oh * ow;
        for y in 0..oh {
            let sy = y / 2;
            let row_src = src + sy * w;
            let row_dst = dst + y * ow;
            for x in 0..ow {
                out.data[row_dst + x] = t.data[row_src + x / 2];
            }
        }
    }
    out
}

/// out = β·a + b（RDB 残差缩放）
fn scaled_add(a: &Tensor, b: &Tensor, beta: f32) -> Tensor {
    debug_assert_eq!((a.c, a.h, a.w), (b.c, b.h, b.w));
    let mut out = Tensor::new(a.c, a.h, a.w);
    for i in 0..out.data.len() {
        out.data[i] = beta * a.data[i] + b.data[i];
    }
    out
}

/// 前向推理：输入 CHW 0-1 → 输出 CHW 0-1（未 clip，clip 由 pipeline 决定）
///
/// blob 槽位竞技场：bottom 读取 → 计算 → 写 top；
/// blob 最后一次消费后释放（cunet 深网内存控制）
pub fn forward(model: &super::model::WaifuModel, input: &Tensor) -> Result<Tensor, String> {
    // last_use[slot] = 最后一次作为 bottom 的层索引（无消费者保持 0 = 立即可释放）
    let mut last_use = vec![0usize; model.n_slots];
    for (i, l) in model.layers.iter().enumerate() {
        for b in l.bottoms() {
            last_use[b] = last_use[b].max(i);
        }
    }

    let mut blobs: Vec<Option<Tensor>> = vec![None; model.n_slots];
    blobs[0] = Some(input.clone());

    fn get<'a>(blobs: &'a [Option<Tensor>], s: usize, i: usize) -> Result<&'a Tensor, String> {
        blobs
            .get(s)
            .and_then(|t| t.as_ref())
            .ok_or_else(|| format!("层 {i} 槽位 {s} 无数据"))
    }

    for (i, layer) in model.layers.iter().enumerate() {
        let out = match layer {
            ExecLayer::Conv {
                weight,
                bias,
                out_ch,
                kernel,
                pad,
                stride,
                dilation,
                leaky_slope,
                bottom,
                ..
            } => {
                let inp = get(&blobs, *bottom, i)?;
                conv2d(
                    inp,
                    weight,
                    bias,
                    *out_ch,
                    *kernel,
                    *pad,
                    *stride,
                    *dilation,
                    *leaky_slope,
                )
            }
            ExecLayer::Deconv {
                weight,
                bias,
                out_ch,
                kernel,
                stride,
                pad,
                leaky_slope,
                bottom,
                ..
            } => {
                let inp = get(&blobs, *bottom, i)?;
                deconv2d(
                    inp,
                    weight,
                    bias,
                    *out_ch,
                    *kernel,
                    *stride,
                    *pad,
                    *leaky_slope,
                )
            }
            ExecLayer::GlobalAvgPool { bottom, .. } => global_avg_pool(get(&blobs, *bottom, i)?),
            ExecLayer::Sigmoid { bottom, .. } => sigmoid(get(&blobs, *bottom, i)?),
            ExecLayer::Scale { data, vec, .. } => {
                scale_ch(get(&blobs, *data, i)?, get(&blobs, *vec, i)?)
            }
            ExecLayer::Axpy { alpha, x, y, .. } => axpy(
                get(&blobs, *alpha, i)?,
                get(&blobs, *x, i)?,
                get(&blobs, *y, i)?,
            ),
            ExecLayer::CropCenter {
                crop_h,
                crop_w,
                bottom,
                ..
            } => crop_center(get(&blobs, *bottom, i)?, *crop_h, *crop_w),
            ExecLayer::Crop {
                offset,
                bottom,
                reference,
                ..
            } => {
                let data = get(&blobs, *bottom, i)?;
                let r = get(&blobs, *reference, i)?;
                crop_ref(data, r, *offset)
            }
            ExecLayer::EltwiseAdd { a, b, .. } => {
                let ta = get(&blobs, *a, i)?;
                let tb = get(&blobs, *b, i)?;
                if (ta.c, ta.h, ta.w) != (tb.c, tb.h, tb.w) {
                    return Err(format!(
                        "层 {i} eltwise 形状错配: {},{},{} vs {},{},{}",
                        ta.c, ta.h, ta.w, tb.c, tb.h, tb.w
                    ));
                }
                eltwise_add(ta, tb)
            }
            ExecLayer::Concat { bottoms, .. } => {
                let inputs: Vec<&Tensor> = bottoms
                    .iter()
                    .map(|b| get(&blobs, *b, i))
                    .collect::<Result<_, _>>()?;
                concat_ch(&inputs).map_err(|e| format!("层 {i} concat: {}", e))?
            }
            ExecLayer::NearestUp2 { bottom, .. } => {
                let t = get(&blobs, *bottom, i)?;
                nearest_up2(t)
            }
            ExecLayer::ScaledAdd { a, b, beta, .. } => {
                let ta = get(&blobs, *a, i)?;
                let tb = get(&blobs, *b, i)?;
                if (ta.c, ta.h, ta.w) != (tb.c, tb.h, tb.w) {
                    return Err(format!(
                        "层 {i} scaled-add 形状错配: {},{},{} vs {},{},{}",
                        ta.c, ta.h, ta.w, tb.c, tb.h, tb.w
                    ));
                }
                scaled_add(ta, tb, *beta)
            }
        };
        let top = layer.top();
        // 调试：纯色输入下逐层【通道内空间】平坦性检查（第一个空间非平坦层 = 问题层）
        {
            let mut spread: f32 = 0.0;
            for c in 0..out.c {
                let base = c * out.h * out.w;
                let slice = &out.data[base..base + out.h * out.w];
                if let Some(mn) = slice.iter().copied().reduce(f32::min) {
                    if let Some(mx) = slice.iter().copied().reduce(f32::max) {
                        spread = spread.max(mx - mn);
                    }
                }
            }
            // 调试：DC 追踪（每层 ch0 首值）
            {
                let kind = match layer {
                    ExecLayer::Conv { out_ch, .. } => format!("Conv->{out_ch}"),
                    ExecLayer::Deconv { out_ch, .. } => format!("Deconv->{out_ch}"),
                    ExecLayer::GlobalAvgPool { .. } => "GAP".into(),
                    ExecLayer::Sigmoid { .. } => "Sigmoid".into(),
                    ExecLayer::Scale { .. } => "Scale".into(),
                    ExecLayer::Axpy { .. } => "Axpy".into(),
                    ExecLayer::CropCenter { .. } => "CropCenter".into(),
                    ExecLayer::Crop { .. } => "Crop".into(),
                    ExecLayer::EltwiseAdd { .. } => "EltwiseAdd".into(),
                    ExecLayer::Concat { .. } => "Concat".into(),
                    ExecLayer::NearestUp2 { .. } => "NearestUp2".into(),
                    ExecLayer::ScaledAdd { .. } => "ScaledAdd".into(),
                };
                eprintln!(
                    "[dbg] 层 {i} {kind}: ch0={:.6} ch1={:.6} ch2={:.6}",
                    out.data[0],
                    out.data[out.h * out.w.min(out.data.len() - 1)].max(0.0),
                    out.data.get(2 * out.h * out.w).copied().unwrap_or(0.0)
                );
            }
            if spread.abs() > 1e-5 {
                let kind = match layer {
                    ExecLayer::Conv {
                        kernel,
                        stride,
                        pad,
                        out_ch,
                        ..
                    } => format!("Conv k{kernel} s{stride} p{pad} -> {out_ch}ch"),
                    ExecLayer::Deconv {
                        kernel,
                        stride,
                        pad,
                        out_ch,
                        ..
                    } => format!("Deconv k{kernel} s{stride} p{pad} -> {out_ch}ch"),
                    ExecLayer::GlobalAvgPool { .. } => "GAP".into(),
                    ExecLayer::Sigmoid { .. } => "Sigmoid".into(),
                    ExecLayer::Scale { .. } => "Scale".into(),
                    ExecLayer::Axpy { .. } => "Axpy".into(),
                    ExecLayer::CropCenter { crop_h, crop_w, .. } => {
                        format!("CropCenter {crop_h}x{crop_w}")
                    }
                    ExecLayer::Crop { offset, .. } => format!("Crop off{offset}"),
                    ExecLayer::EltwiseAdd { .. } => "EltwiseAdd".into(),
                    ExecLayer::Concat { bottoms, .. } => format!("Concat x{}", bottoms.len()),
                    ExecLayer::NearestUp2 { .. } => "NearestUp2".into(),
                    ExecLayer::ScaledAdd { .. } => "ScaledAdd".into(),
                };
                eprintln!(
                    "[dbg] 层 {i} {kind} 空间非平坦: spread={spread:.6} ({}x{}x{})",
                    out.c, out.h, out.w
                );
            }
        }
        blobs[top] = Some(out);
        // 释放本层消费完的 bottom（最后一次消费）
        for b in layer.bottoms() {
            if b != top && last_use[b] == i {
                blobs[b] = None;
            }
        }
    }

    blobs
        .get_mut(model.output_slot)
        .and_then(|t| t.take())
        .ok_or_else(|| "网络无输出".to_string())
}
