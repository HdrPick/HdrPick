//! 推理编排：图像进出 / padding / clip / 任意倍率迭代（P1：CPU 整图）

use super::cpu::{self, Tensor};
use super::model::{ModelKind, WaifuModel};

/// 模型根目录（原版模型文件直接复用）
pub fn default_model_root() -> std::path::PathBuf {
    // exe 同目录 models/（开发期指向原成品目录）
    let exe = std::env::current_exe().unwrap_or_default();
    let beside = exe
        .parent()
        .map(|p| p.join("models"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| std::path::PathBuf::from(r"e:\jietu\图片放大器\models"));
    beside
}

/// RGB8 图 → CHW f32 [0,1] Tensor
pub fn image_to_tensor(img: &image::RgbImage) -> Tensor {
    let (w, h) = img.dimensions();
    let mut t = Tensor::new(3, h as usize, w as usize);
    for (x, y, p) in img.enumerate_pixels() {
        let [r, g, b] = p.0;
        t.set(0, y as usize, x as usize, r as f32 / 255.0);
        t.set(1, y as usize, x as usize, g as f32 / 255.0);
        t.set(2, y as usize, x as usize, b as f32 / 255.0);
    }
    t
}

/// RGBA8 图 → (RGB 3ch tensor, alpha 1ch tensor 或 None[全不透明])
///
/// alpha 保留语义（对齐原版 stImage::HasAlpha）：存在半透明像素才走 alpha 路径
pub fn rgba_to_tensors(img: &image::RgbaImage) -> (Tensor, Option<Tensor>) {
    let (w, h) = img.dimensions();
    let mut rgb = Tensor::new(3, h as usize, w as usize);
    let mut alpha = Tensor::new(1, h as usize, w as usize);
    let mut has_alpha = false;
    for (x, y, p) in img.enumerate_pixels() {
        let [r, g, b, a] = p.0;
        rgb.set(0, y as usize, x as usize, r as f32 / 255.0);
        rgb.set(1, y as usize, x as usize, g as f32 / 255.0);
        rgb.set(2, y as usize, x as usize, b as f32 / 255.0);
        if a != 255 {
            has_alpha = true;
        }
        alpha.set(0, y as usize, x as usize, a as f32 / 255.0);
    }
    (rgb, if has_alpha { Some(alpha) } else { None })
}

/// 单通道 alpha 复制成 [a,a,a] 3 通道（过 RGB 网络）
fn alpha_to_rgb3(a: &Tensor) -> Tensor {
    let mut t = Tensor::new(3, a.h, a.w);
    for c in 0..3 {
        let base = c * a.h * a.w;
        t.data[base..base + a.h * a.w].copy_from_slice(&a.data);
    }
    t
}

/// 3 通道取 ch0 还原单通道 alpha
fn rgb3_to_alpha(t: &Tensor) -> Tensor {
    let mut a = Tensor::new(1, t.h, t.w);
    a.data.copy_from_slice(&t.data[..t.h * t.w]);
    a
}

/// CHW f32 Tensor（未 clip）→ clip [0,1] ×255 → RGB8
pub fn tensor_to_image(t: &Tensor) -> image::RgbImage {
    let mut img = image::RgbImage::new(t.w as u32, t.h as u32);
    for y in 0..t.h {
        for x in 0..t.w {
            let px = |c: usize| {
                let v = t.at(c, y, x);
                let v = v.clamp(0.0, 1.0);
                (v * 255.0 + 0.5).min(255.0) as u8
            };
            img.put_pixel(x as u32, y as u32, image::Rgb([px(0), px(1), px(2)]));
        }
    }
    img
}

/// BORDER_REPLICATE padding（上下左右各 pad 像素，复制边缘）
pub fn pad_replicate(t: &Tensor, pad: usize) -> Tensor {
    if pad == 0 {
        return Tensor {
            c: t.c,
            h: t.h,
            w: t.w,
            data: t.data.clone(),
        };
    }
    let (c, h, w) = (t.c, t.h, t.w);
    let (nh, nw) = (h + 2 * pad, w + 2 * pad);
    let mut out = Tensor::new(c, nh, nw);
    for ch in 0..c {
        for y in 0..nh {
            let sy = y.saturating_sub(pad).min(h - 1);
            for x in 0..nw {
                let sx = x.saturating_sub(pad).min(w - 1);
                let v = t.at(ch, sy, sx);
                out.set(ch, y, x, v);
            }
        }
    }
    out
}

/// 整图推理一次（scale / noise 通用）
///
/// 流程与原版对齐（cNet::ReconstructImage）：
/// pad(offset) 每边 → forward → 中心裁剪有效区
///
/// 两种网络输出语义（实测原版行为）：
/// - scale 家族（含 noise_scale 合并模型）：输出含 padding → 裁剪 [offset, offset + size×inner)
/// - noise-only 模型（inner_scale=1，如 cunet noise{N}_model）：网络尾层已裁掉
///   padding，输出 = 净尺寸 → 直接取 [0, size)
pub fn run_once(model: &WaifuModel, input: &Tensor) -> Result<Tensor, String> {
    if input.c != model.in_ch {
        return Err(format!(
            "通道不匹配: 模型 {} vs 图像 {}",
            model.in_ch, input.c
        ));
    }
    let padded = pad_replicate(input, model.net_offset);
    let out = cpu::forward(model, &padded)?;
    // 中心裁剪：有效区 = [offset, offset + 原尺寸×inner_scale)
    let (oh, ow) = (input.h * model.inner_scale, input.w * model.inner_scale);
    let (y0, x0) = crop_origin(model, &out, oh, ow)?;
    let mut crop = Tensor::new(out.c, oh, ow);
    for ch in 0..out.c {
        for y in 0..oh {
            for x in 0..ow {
                crop.set(ch, y, x, out.at(ch, y0 + y, x0 + x));
            }
        }
    }
    Ok(crop)
}

/// 裁剪起点解析：输出含 padding → (offset, offset)；网络已自裁（noise-only）→ (0, 0)
fn crop_origin(
    model: &WaifuModel,
    out: &Tensor,
    oh: usize,
    ow: usize,
) -> Result<(usize, usize), String> {
    crop_origin_at(model.net_offset, out, oh, ow)
}

/// 显式偏移版（GPU 分块边距裁剪用）
fn crop_origin_at(o: usize, out: &Tensor, oh: usize, ow: usize) -> Result<(usize, usize), String> {
    if out.h >= o + oh && out.w >= o + ow {
        Ok((o, o))
    } else if out.h >= oh && out.w >= ow {
        // noise-only 模型：网络输出 = 净尺寸（尾层 Crop 已去 padding）
        Ok((0, 0))
    } else {
        Err(format!(
            "输出尺寸不足: 输出 {}x{} 需 {}x{}（offset {}）",
            out.w, out.h, ow, oh, o
        ))
    }
}

/// Real-ESRGAN（net_offset=0）的 tile 重叠边距（输入像素）
///
/// 0 边距时 conv 在 tile 边缘 zero-pad → 每块边界出现隐约接缝（暗/亮框）；
/// waifu2x 家族 net_offset≥7 自带边距无此问题。16px 上下文后中心裁剪，
/// 残余接缝不可见（对拍口径与 ncnn ≥20px 边界排除一致）。
const RRDB_TILE_MARGIN: usize = 16;

/// tile 提取边距：waifu2x 用 net_offset，Real-ESRGAN 用固定重叠
fn tile_pad(model: &WaifuModel) -> usize {
    if model.net_offset > 0 {
        model.net_offset
    } else {
        RRDB_TILE_MARGIN
    }
}

/// 完整放大（P1：scale 模式，任意倍率 = N×2x + 最终最近邻缩放）
///
/// 注意：原版非整数倍率时最后用 cv::resize（默认双三次 INTER_CUBIC），
/// P1 对拍用 2x 整数倍（不走缩放路径）
pub fn upscale_image(
    model_root: &std::path::Path,
    kind: ModelKind,
    input: &image::RgbImage,
    iterations: usize,
) -> Result<image::RgbImage, String> {
    let model = WaifuModel::load(kind, model_root, kind.scale_file())?;
    let mut t = image_to_tensor(input);
    for i in 0..iterations {
        log::info!(
            "[upscale] 迭代 {}/{}（{}×{} → ×{}）",
            i + 1,
            iterations,
            t.w,
            t.h,
            model.inner_scale
        );
        t = run_once(&model, &t)?;
    }
    Ok(tensor_to_image(&t))
}

// ==================== P2/P5：GPU 路径（分块 + 拼接） ====================

/// GPU 默认 tile（内存权衡：upconv_7 最大层 256ch×(tile+28)²×4B）
const GPU_TILE: usize = 256;
/// 会话纹理显存上限（超限降 tile：cunet 深网 tile 256 约 300-500MB，必要时减半）
const GPU_SESSION_MEM_LIMIT: f64 = 600.0;

/// 估算 tile 对应的会话峰值显存（MB）
///
/// 峰值活跃槽位法：模拟 blob 生命周期（bottom 最后一次消费后释放），
/// 逐层统计活跃槽位字节和的最大值——与 GpuSession 的空闲池复用实际分配一致。
/// （旧法为全部槽位总和：对 RRDBNet 这类 900+ 槽深网严重高估，会把 tile 压得过小）
fn estimate_session_mb(model: &WaifuModel, tile: usize) -> f64 {
    let pad = tile_pad(model);
    let pw = tile + 2 * pad;
    let ph = tile + 2 * pad;
    let Ok(shapes) = model.slot_shapes(pw, ph) else {
        return f64::MAX;
    };
    let slot_bytes = |s: usize| -> f64 {
        let (w, h, c) = shapes[s];
        (w as f64) * (h as f64) * (c.div_ceil(4) as f64) * 16.0
    };
    // last_use 与 forward 相同的释放语义
    let mut last_use = vec![0usize; model.n_slots];
    for (i, l) in model.layers.iter().enumerate() {
        for b in l.bottoms() {
            last_use[b] = last_use[b].max(i);
        }
    }
    let mut active: Vec<usize> = vec![0]; // 槽 0（输入）常驻
    let mut peak: f64 = slot_bytes(0);
    for (i, l) in model.layers.iter().enumerate() {
        active.push(l.top());
        for b in l.bottoms() {
            if last_use[b] == i {
                active.retain(|&s| s != b);
            }
        }
        let bytes: f64 = active.iter().map(|&s| slot_bytes(s)).sum();
        peak = peak.max(bytes);
    }
    peak / 1024.0 / 1024.0
}

/// 按显存预算选择 tile 尺寸（256 → 128 → 96 → 64）
fn pick_tile(model: &WaifuModel) -> usize {
    let mut tile = GPU_TILE;
    while tile > 64 && estimate_session_mb(model, tile) > GPU_SESSION_MEM_LIMIT {
        tile = if tile > 128 { 128 } else { tile / 2 };
    }
    tile
}

/// 提取 (tx, ty) 起点 tile_w×tile_h 的 replicate-pad 块（pad = offset）
fn extract_padded_tile(
    t: &Tensor,
    tx: usize,
    ty: usize,
    tile_w: usize,
    tile_h: usize,
    offset: usize,
) -> Tensor {
    let pw = tile_w + 2 * offset;
    let ph = tile_h + 2 * offset;
    let mut out = Tensor::new(t.c, ph, pw);
    for c in 0..t.c {
        for y in 0..ph {
            // 源行：y + ty - offset，clamp 到图像界内（replicate）
            let sy = (y + ty).saturating_sub(offset).min(t.h - 1);
            for x in 0..pw {
                let sx = (x + tx).saturating_sub(offset).min(t.w - 1);
                out.set(c, y, x, t.at(c, sy, sx));
            }
        }
    }
    out
}

/// GPU 单次放大（分块）：整图 Tensor → ×inner_scale Tensor
///
/// 返回 (输出, 耗时 ms)；GPU 不可用时返回 Err（调用方回落 CPU）
pub fn run_once_gpu(model: &WaifuModel, input: &Tensor) -> Result<(Tensor, u128), String> {
    let engine_guard = super::d3d11::engine();
    let sync_engine = engine_guard
        .as_ref()
        .map_err(|e| format!("GPU 引擎不可用: {}", e))?;
    let t0 = std::time::Instant::now();

    let tile_w = pick_tile(model).min(input.w.max(1)).max(32);
    let tile_h = pick_tile(model).min(input.h.max(1)).max(32);

    let mut engine = sync_engine.0.lock().map_err(|_| "GPU 锁中毒")?;
    // tile 重叠边距：waifu2x = net_offset（裁剪语义天然含边距）；
    // Real-ESRGAN = 16px 重叠（0 边距时 conv 在块边缘 zero-pad → 接缝）
    let pad = tile_pad(model);
    // 会话入参 = tile + 2×pad（session 内部再加工 net_offset；RRDB offset=0 → 直传）
    let extra = pad - model.net_offset;
    // 会话按 (sw, sh) 缓存复用（≤4 种组合：主块/右缘/下缘/右下角）。
    // 此前逐 tile 比较尺寸、不同即整体重建——行优先遍历时每行末尾的
    // 边缘块与下一行开头的主块来回切换，每行推倒重建两次会话，
    // 深网（Real-ESRGAN 837 层）一次会话分配即 GB 级，严重拖慢且浪费。
    let mut sessions: Vec<((usize, usize), super::d3d11::GpuSession)> = Vec::new();

    let out_w = input.w * model.inner_scale;
    let out_h = input.h * model.inner_scale;
    let mut out = Tensor::new(input.c, out_h, out_w);

    let tiles_x = input.w.div_ceil(tile_w);
    let tiles_y = input.h.div_ceil(tile_h);
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let tw = tile_w.min(input.w - tx * tile_w);
            let th = tile_h.min(input.h - ty * tile_h);
            let (sw, sh) = (tw + 2 * extra, th + 2 * extra);
            let sess = match sessions.iter_mut().find(|((w, h), _)| *w == sw && *h == sh) {
                Some((_, s)) => s,
                None => {
                    let s = super::d3d11::GpuSession::new(&engine, model, sw, sh)?;
                    sessions.push(((sw, sh), s));
                    &mut sessions.last_mut().expect("just pushed").1
                }
            };
            // 边距内 replicate 取自全图界限（clamp 到图像边），相邻块上下文衔接
            let padded = extract_padded_tile(input, tx * tile_w, ty * tile_h, tw, th, pad);
            let raw = sess.run_tile(&padded)?;
            // 中心裁剪 [o, o + tw*inner_scale) → 拼进输出
            // o：waifu2x = net_offset（黄金对拍语义）；RRDB = pad×inner（重叠区）
            // （noise-only 模型网络输出=净尺寸 → crop_origin 自适应到 (0,0)）
            let (cw, chh) = (tw * model.inner_scale, th * model.inner_scale);
            let o = if model.net_offset > 0 {
                model.net_offset
            } else {
                pad * model.inner_scale
            };
            let (y0, x0) = crop_origin_at(o, &raw, chh, cw)?;
            let (dx, dy) = (
                tx * tile_w * model.inner_scale,
                ty * tile_h * model.inner_scale,
            );
            for c in 0..out.c {
                for y in 0..chh {
                    for x in 0..cw {
                        out.set(c, dy + y, dx + x, raw.at(c, y0 + y, x0 + x));
                    }
                }
            }
        }
    }
    // 会话 drop（纹理 Release）后 Flush：触发 D3D11 延迟析构立即归还内存，
    // 否则核显共享内存的分配会长期挂在进程工作集上（此前 20GB 不落的另一半原因）
    drop(sessions);
    engine.flush();
    log::info!(
        "[upscale/GPU] {}×{} → {}×{}（{} tile），耗时 {:.0}ms",
        input.w,
        input.h,
        out_w,
        out_h,
        tiles_x * tiles_y,
        t0.elapsed().as_millis()
    );
    Ok((out, t0.elapsed().as_millis()))
}

/// 双路放大：GPU 优先（不可用/含 Eltwise 自动回落 CPU）
///
/// noise_level: Some(0-3) 时用降噪+放大合并模型（upconv_7/upresnet10 家族）
///
/// 返回 (输出, 是否使用了 GPU)
pub fn upscale_image_auto(
    model_root: &std::path::Path,
    kind: ModelKind,
    input: &image::RgbImage,
    iterations: usize,
    noise_level: Option<u32>,
) -> Result<(image::RgbImage, bool), String> {
    // 模型选择：降噪合并模型优先（文件存在时），否则纯 scale 模型
    let model_file = match noise_level {
        Some(n) if kind.has_noise_scale_model() => {
            let f = kind.noise_scale_file(n);
            let p = model_root.join(kind.dir_name()).join(&f);
            if p.exists() {
                f
            } else {
                log::warn!("[upscale] 降噪模型缺失 {} → 回落纯放大", p.display());
                kind.scale_file().to_string()
            }
        }
        _ => kind.scale_file().to_string(),
    };
    let model = WaifuModel::load(kind, model_root, &model_file)?;
    let mut t = image_to_tensor(input);

    let mut use_gpu = false;
    for i in 0..iterations {
        if !use_gpu {
            match run_once_gpu(&model, &t) {
                Ok((out, _)) => {
                    use_gpu = true;
                    t = out;
                    continue;
                }
                Err(e) => {
                    log::info!("[upscale] GPU 路径不可用，回落 CPU: {}", e);
                }
            }
        }
        log::info!("[upscale/CPU] 迭代 {}/{}", i + 1, iterations);
        t = run_once(&model, &t)?;
    }
    Ok((tensor_to_image(&t), use_gpu))
}

// ==================== P3：TTA（8 面体变换平均） ====================

/// 二面体变换 D4（8 种）：identity / hflip / vflip / rot180 / transpose / anti-transpose
/// 编码：bit0=hflip, bit1=vflip, bit2=transpose（先翻转后转置）
fn tta_transform(t: &Tensor, code: u8) -> Tensor {
    let (w, h) = (t.w, t.h);
    let (hflip, vflip, transpose) = (code & 1 != 0, code & 2 != 0, code & 4 != 0);
    // 翻转后尺寸
    let (fw, fh) = (w, h); // 翻转不改尺寸
    let (ow, oh) = if transpose { (fh, fw) } else { (fw, fh) };
    let mut out = Tensor::new(t.c, oh, ow);
    for c in 0..t.c {
        for y in 0..h {
            for x in 0..w {
                let v = t.at(c, y, x);
                // 源坐标 → 翻转
                let (mut sx, mut sy) = (x, y);
                if hflip {
                    sx = w - 1 - sx;
                }
                if vflip {
                    sy = h - 1 - sy;
                }
                // 转置（x,y 互换）
                let (ox, oy) = if transpose { (sy, sx) } else { (sx, sy) };
                out.set(c, oy, ox, v);
            }
        }
    }
    out
}

/// 逆变换（D4 群元素自逆或成对）：transpose∘flip 的逆 = flip'∘transpose
/// 实现：直接对变换再求逆——遍历求逆映射即可（等价于对 code 求逆置换）
fn tta_inverse(t: &Tensor, code: u8) -> Tensor {
    // D4 逆映射：对每个输出坐标找源。等价技巧：逆(code) = code 对应的逆元素。
    // 8 元素循环群结构：identity/rot180/hflip/vflip 自逆；transpose⁻¹=transpose；
    // 主反对角线互换成对。直接用变换本身的"逆序"枚举：
    // 由于 tta_transform 按"先翻转后转置"定义，其逆 = "先逆转置后逆翻转"。
    let (w, h) = (t.w, t.h);
    let (hflip, vflip, transpose) = (code & 1 != 0, code & 2 != 0, code & 4 != 0);
    // 逆：先逆转置（若 transpose：out(x,y)=in(y,x)），再逆翻转
    let (iw, ih) = if transpose { (h, w) } else { (w, h) };
    let mut out = Tensor::new(t.c, ih, iw);
    for c in 0..t.c {
        for y in 0..h {
            for x in 0..w {
                let v = t.at(c, y, x);
                // 逆转置
                let (mut ux, mut uy) = if transpose { (y, x) } else { (x, y) };
                // 逆翻转
                if hflip {
                    ux = iw - 1 - ux;
                }
                if vflip {
                    uy = ih - 1 - uy;
                }
                out.set(c, uy, ux, v);
            }
        }
    }
    out
}

/// TTA 单次放大（GPU/CPU 双路）：8 面体变换推理平均
///
/// 开销 ×8，质量略提升（原版 use_tta 等价语义）
pub fn run_once_tta(model: &WaifuModel, input: &Tensor) -> Result<Tensor, String> {
    let mut acc: Option<Tensor> = None;
    let n = 8usize;
    for code in 0..n {
        let transformed = tta_transform(input, code as u8);
        let out = match run_once_gpu(model, &transformed) {
            Ok((o, _)) => o,
            Err(_) => run_once(model, &transformed)?,
        };
        let back = tta_inverse(&out, code as u8);
        acc = Some(match acc {
            None => back,
            Some(mut a) => {
                for (d, s) in a.data.iter_mut().zip(back.data.iter()) {
                    *d += *s;
                }
                a
            }
        });
    }
    let mut out = acc.ok_or("TTA 无输出")?;
    let inv = 1.0 / n as f32;
    for v in out.data.iter_mut() {
        *v *= inv;
    }
    Ok(out)
}

/// 放大输出（f32 [0,1] 域，未量化——供 8/16bit/HDR 多格式编码）
#[derive(Clone)]
pub struct UpscaleOutput {
    /// RGB 3 通道（sRGB 编码值域）
    pub rgb: Tensor,
    /// alpha 1 通道（线性不透明度；None = 全不透明）
    pub alpha: Option<Tensor>,
}

/// 单次模型推理（GPU 优先 / TTA 可选；used_gpu 状态透传）
fn run_model_once(
    m: &WaifuModel,
    t: &Tensor,
    use_tta: bool,
    used_gpu: &mut bool,
    first: bool,
) -> Result<Tensor, String> {
    if use_tta {
        *used_gpu = true;
        return run_once_tta(m, t);
    }
    match run_once_gpu(m, t) {
        Ok((out, _)) => {
            *used_gpu = true;
            Ok(out)
        }
        Err(e) => {
            if first {
                log::info!("[upscale] GPU 不可用回落 CPU: {}", e);
            }
            run_once(m, t)
        }
    }
}

/// 目标尺寸倍率解析（原版 CalcScaleRatio 语义）
///
/// - 双指定宽高：取 max(w/iw, h/ih)（覆盖式，短边撑满裁余）
/// - 单指定宽/高：按该边换算
pub fn calc_scale_ratio(
    scale: Option<f64>,
    target_w: Option<u32>,
    target_h: Option<u32>,
    iw: u32,
    ih: u32,
) -> f32 {
    if let Some(s) = scale {
        return s as f32;
    }
    let sw = target_w.map(|w| w as f32 / iw.max(1) as f32);
    let sh = target_h.map(|h| h as f32 / ih.max(1) as f32);
    match (sw, sh) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 1.0,
    }
}

/// 任意倍率放大（原版 waifu2x-caffe 等效语义，RGBA alpha 保留）
///
/// 1.0 < scale ≤ 16.0：N 次 inner_scale 推理（N = ceil(log_scale/log_inner)，
/// 原版 waifu2x.cpp:950 公式；2x 模型 log₂、Real-ESRGAN ×4 模型 log₄），
/// 末次超采样后 CatmullRom（双三次）回缩到精确目标（原版 cv::resize INTER_CUBIC 等效）。
/// 降噪（合并模型）仅 RGB 首轮应用；alpha 全程过纯 scale 模型（原版 ReconstructScale：
/// alpha 不参与降噪——半透明边缘不受噪声模型影响）。
pub fn upscale_image_exact(
    model_root: &std::path::Path,
    kind: ModelKind,
    input: &image::RgbaImage,
    scale: f32,
    noise_level: Option<u32>,
    use_tta: bool,
) -> Result<(UpscaleOutput, bool), String> {
    if !(1.0..=16.0).contains(&scale) || scale <= 1.0 {
        return Err(format!("倍率需在 (1.0, 16.0] 区间: {}", scale));
    }

    let scale_model = WaifuModel::load(kind, model_root, kind.scale_file())?;
    // 迭代次数：ceil(ln(scale)/ln(inner))——inner=2 即原版 log₂ 公式
    let iters = ((scale.ln() / (scale_model.inner_scale as f32).ln()).ceil() as usize).max(1);
    // 首轮模型（RGB）：降噪合并模型优先（文件存在时），否则纯 scale
    let first_model = match noise_level {
        Some(n) if kind.has_noise_scale_model() => {
            let f = kind.noise_scale_file(n);
            if model_root.join(kind.dir_name()).join(&f).exists() {
                WaifuModel::load(kind, model_root, &f)?
            } else {
                log::warn!("[upscale] 降噪模型缺失 {} → 回落纯放大", f);
                scale_model.clone()
            }
        }
        _ => scale_model.clone(),
    };

    let (mut rgb_t, alpha_t) = rgba_to_tensors(input);
    let mut used_gpu = false;

    // RGB：首轮（可含降噪）+ 后续轮纯 scale
    for i in 0..iters {
        let m = if i == 0 { &first_model } else { &scale_model };
        rgb_t = run_model_once(m, &rgb_t, use_tta, &mut used_gpu, i == 0)?;
    }
    // alpha：全程纯 scale 模型（不降噪；原版 ReconstructScale 语义）
    let mut alpha_t = match alpha_t {
        Some(a) => {
            let mut cur = alpha_to_rgb3(&a);
            for i in 0..iters {
                cur = run_model_once(&scale_model, &cur, use_tta, &mut used_gpu, i == 0)?;
            }
            Some(rgb3_to_alpha(&cur))
        }
        None => None,
    };

    // 精确倍率回缩（f32 域，整数倍时零缩放跳过）
    let tw = ((input.width() as f32 * scale).round() as u32).max(1);
    let th = ((input.height() as f32 * scale).round() as u32).max(1);
    if (rgb_t.w as u32, rgb_t.h as u32) != (tw, th) {
        let mut f32img = tensors_to_f32_image(&rgb_t, alpha_t.as_ref());
        f32img = image::imageops::resize(&f32img, tw, th, image::imageops::FilterType::CatmullRom);
        let (r2, a2) = f32_image_to_tensors(&f32img);
        rgb_t = r2;
        alpha_t = a2;
        log::info!("[upscale] 回缩 → {}×{}", tw, th);
    }

    Ok((
        UpscaleOutput {
            rgb: rgb_t,
            alpha: alpha_t,
        },
        used_gpu,
    ))
}

/// 纯降噪（不放大）：noise{N} 模型（inner_scale=1）单次推理，尺寸不变
///
/// alpha 不过网络（原版 ReconstructNoise 不处理 alpha），原样保留
pub fn denoise_only(
    model_root: &std::path::Path,
    kind: ModelKind,
    input: &image::RgbaImage,
    noise_level: u32,
    use_tta: bool,
) -> Result<(UpscaleOutput, bool), String> {
    if kind.is_realesrgan() {
        return Err("Real-ESRGAN 不支持纯降噪模式（×4 模型已内置真实退化修复）".into());
    }
    let f = kind.noise_file(noise_level);
    let p = model_root.join(kind.dir_name()).join(&f);
    if !p.exists() {
        return Err(format!("降噪模型缺失: {}", p.display()));
    }
    let model = WaifuModel::load(kind, model_root, &f)?;
    let (mut rgb_t, alpha_t) = rgba_to_tensors(input);
    let mut used_gpu = false;
    rgb_t = run_model_once(&model, &rgb_t, use_tta, &mut used_gpu, true)?;
    Ok((
        UpscaleOutput {
            rgb: rgb_t,
            alpha: alpha_t,
        },
        used_gpu,
    ))
}

/// RGB+alpha tensor → RGBA f32 交织图（CatmullRom 回缩用）
fn tensors_to_f32_image(rgb: &Tensor, alpha: Option<&Tensor>) -> image::Rgba32FImage {
    let (w, h) = (rgb.w as u32, rgb.h as u32);
    let mut img = image::Rgba32FImage::new(w, h);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let a = alpha.map(|t| t.at(0, y, x)).unwrap_or(1.0);
            img.put_pixel(
                x as u32,
                y as u32,
                image::Rgba([
                    rgb.at(0, y, x).clamp(0.0, 1.0),
                    rgb.at(1, y, x).clamp(0.0, 1.0),
                    rgb.at(2, y, x).clamp(0.0, 1.0),
                    a.clamp(0.0, 1.0),
                ]),
            );
        }
    }
    img
}

/// RGBA f32 图 → (RGB tensor, alpha tensor[全 1 → None])
fn f32_image_to_tensors(img: &image::Rgba32FImage) -> (Tensor, Option<Tensor>) {
    let (w, h) = img.dimensions();
    let mut rgb = Tensor::new(3, h as usize, w as usize);
    let mut alpha = Tensor::new(1, h as usize, w as usize);
    let mut has_alpha = false;
    for (x, y, p) in img.enumerate_pixels() {
        let [r, g, b, a] = p.0;
        rgb.set(0, y as usize, x as usize, r);
        rgb.set(1, y as usize, x as usize, g);
        rgb.set(2, y as usize, x as usize, b);
        if a < 1.0 {
            has_alpha = true;
        }
        alpha.set(0, y as usize, x as usize, a);
    }
    (rgb, if has_alpha { Some(alpha) } else { None })
}

/// 输出格式
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Png8,
    Png16,
    /// HDR PNG（16bit PQ + cICP BT.2020）
    PngHdr,
    /// OpenEXR（32bit 浮点 scRGB 线性光）
    Exr,
    /// JXL HDR 16bit（精度优先）
    JxlHdr16,
    /// JXL HDR 12bit（体积优先；libjxl 支持任意整数位深）
    JxlHdr12,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "png" | "png8" => Some(OutputFormat::Png8),
            "png16" => Some(OutputFormat::Png16),
            "png_hdr" | "pngHdr" => Some(OutputFormat::PngHdr),
            "exr" => Some(OutputFormat::Exr),
            "jxl" | "jxl16" | "jxl_hdr" => Some(OutputFormat::JxlHdr16),
            "jxl12" => Some(OutputFormat::JxlHdr12),
            _ => None,
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            OutputFormat::Png8 | OutputFormat::Png16 | OutputFormat::PngHdr => "png",
            OutputFormat::Exr => "exr",
            OutputFormat::JxlHdr16 | OutputFormat::JxlHdr12 => "jxl",
        }
    }
}

impl UpscaleOutput {
    /// 量化为 RGBA8
    pub fn to_rgba8(&self) -> image::RgbaImage {
        let t = &self.rgb;
        let mut img = image::RgbaImage::new(t.w as u32, t.h as u32);
        for y in 0..t.h {
            for x in 0..t.w {
                let px = |c: usize| -> u8 {
                    let v = t.at(c, y, x).clamp(0.0, 1.0);
                    (v * 255.0 + 0.5).min(255.0) as u8
                };
                let a = match &self.alpha {
                    Some(al) => (al.at(0, y, x).clamp(0.0, 1.0) * 255.0 + 0.5).min(255.0) as u8,
                    None => 255,
                };
                img.put_pixel(x as u32, y as u32, image::Rgba([px(0), px(1), px(2), a]));
            }
        }
        img
    }

    /// 量化为 RGBA16
    pub fn to_rgba16(&self) -> image::ImageBuffer<image::Rgba<u16>, Vec<u16>> {
        let t = &self.rgb;
        let mut img = image::ImageBuffer::new(t.w as u32, t.h as u32);
        for y in 0..t.h {
            for x in 0..t.w {
                let px = |c: usize| -> u16 {
                    let v = t.at(c, y, x).clamp(0.0, 1.0);
                    (v * 65535.0 + 0.5).min(65535.0) as u16
                };
                let a = match &self.alpha {
                    Some(al) => {
                        (al.at(0, y, x).clamp(0.0, 1.0) * 65535.0 + 0.5).min(65535.0) as u16
                    }
                    None => 65535,
                };
                img.put_pixel(x as u32, y as u32, image::Rgba([px(0), px(1), px(2), a]));
            }
        }
        img
    }

    /// 转为 scRGB f16 CapturedTexture（HDR PNG/EXR 编码入口）
    ///
    /// SDR 模拟还原到 HDR + 画质提升（全图管线）：自适应拐点 + 色域外推 +
    /// 高光扩展 + 过曝缓解（sdr_to_hdr_image）→ nits → /80 转 scRGB 域
    /// （物理定义 1.0=80nits；参考白 200nits → 白=2.5 scRGB）。
    /// 下游：EXR 直通写 scRGB；PNG HDR 经 to_pq_rgb（物理常量 80）→ nits→PQ。
    /// alpha 直通（EXR 编码器当前固定 1.0，PNG HDR 直通）
    fn to_scrgb_texture(
        &self,
        enhance: crate::color::SdrToHdrParams,
    ) -> crate::capture::CapturedTexture {
        let t = &self.rgb;
        let nits_img = crate::color::sdr_to_hdr_image(&t.data, t.w, t.h, &enhance);
        self.scrgb_texture_from_nits(&nits_img)
    }

    /// 预计算 nits（BT.709 线性域 CHW）→ scRGB f16 CapturedTexture（AI 档入口）
    fn scrgb_texture_from_nits(&self, nits_img: &[f32]) -> crate::capture::CapturedTexture {
        let t = &self.rgb;
        let (w, h) = (t.w as u32, t.h as u32);
        let npx = (t.h * t.w) as usize;
        let f16 = crate::viewer::decode::exr::f32_to_f16;
        let mut data = vec![0u8; npx * 8];
        for i in 0..npx {
            let o = i * 8;
            let lin = |c: usize| f16(nits_img[c * npx + i] / 80.0);
            data[o..o + 2].copy_from_slice(&lin(0).to_le_bytes());
            data[o + 2..o + 4].copy_from_slice(&lin(1).to_le_bytes());
            data[o + 4..o + 6].copy_from_slice(&lin(2).to_le_bytes());
            let a = match &self.alpha {
                Some(al) => f16(al.at(0, i / t.w as usize, i % t.w as usize).clamp(0.0, 1.0)),
                None => f16(1.0),
            };
            data[o + 6..o + 8].copy_from_slice(&a.to_le_bytes());
        }
        crate::capture::CapturedTexture {
            width: w,
            height: h,
            format: crate::color::PixelFormat::R16g16b16a16Float,
            data,
            row_pitch: t.w * 8,
            via_gdi: false,
        }
    }

    /// 按格式保存（HDR 格式用默认关闭档：纯 SDR 等效还原）
    pub fn save(&self, path: &std::path::Path, fmt: OutputFormat) -> Result<(), String> {
        self.save_hdr(path, fmt, crate::color::SdrToHdrPreset::Off)
    }

    /// 按格式保存 + SDR→HDR 增强档位（HDR 容器格式生效；SDR 格式忽略）
    ///
    /// 参考白统一取 monitor::effective_sdr_white_nits（HDR 开关/滑块可能中途变化）
    /// AI 档：HDRTVNet ITM 网络推理（itm::enhance_image_nits），不走曲线参数
    pub fn save_hdr(
        &self,
        path: &std::path::Path,
        fmt: OutputFormat,
        preset: crate::color::SdrToHdrPreset,
    ) -> Result<(), String> {
        // 参考白在保存时才读取（HDR 期滑块值或其记忆）
        let enhance = preset.params(crate::capture::monitor::effective_sdr_white_nits());
        // AI 档预计算 nits（网络直出，不走曲线）；其余档位按需计算
        let ai_nits: Option<Vec<f32>> = if preset == crate::color::SdrToHdrPreset::Ai {
            let t0 = std::time::Instant::now();
            let nits = super::itm::enhance_image_nits(&self.rgb.data, self.rgb.w, self.rgb.h)?;
            log::info!(
                "AI 智能档（HDRTVNet AGCM+LE）推理完成: {}x{}, {:.1}s",
                self.rgb.w,
                self.rgb.h,
                t0.elapsed().as_secs_f32()
            );
            Some(nits)
        } else {
            None
        };
        match fmt {
            OutputFormat::Png8 => self
                .to_rgba8()
                .save(path)
                .map_err(|e| format!("PNG 保存失败: {}", e)),
            OutputFormat::Png16 => self
                .to_rgba16()
                .save(path)
                .map_err(|e| format!("PNG16 保存失败: {}", e)),
            OutputFormat::PngHdr => {
                let tex = match &ai_nits {
                    Some(nits) => self.scrgb_texture_from_nits(nits),
                    None => self.to_scrgb_texture(enhance),
                };
                crate::encode::save_hdr_png(&tex, path, &Default::default())
                    .map_err(|e| format!("HDR PNG 保存失败: {}", e))
            }
            OutputFormat::Exr => {
                let tex = match &ai_nits {
                    Some(nits) => self.scrgb_texture_from_nits(nits),
                    None => self.to_scrgb_texture(enhance),
                };
                crate::encode::save_exr(&tex, path).map_err(|e| format!("EXR 保存失败: {}", e))
            }
            OutputFormat::JxlHdr16 | OutputFormat::JxlHdr12 => {
                let depth = match fmt {
                    OutputFormat::JxlHdr12 => 12,
                    _ => 16,
                };
                // RGBA f32 交织
                let t = &self.rgb;
                let npx = t.h * t.w;
                let mut buf = vec![0f32; npx * 4];
                for y in 0..t.h {
                    for x in 0..t.w {
                        let i = (y * t.w + x) * 4;
                        buf[i] = t.at(0, y, x);
                        buf[i + 1] = t.at(1, y, x);
                        buf[i + 2] = t.at(2, y, x);
                        buf[i + 3] = self.alpha.as_ref().map(|a| a.at(0, y, x)).unwrap_or(1.0);
                    }
                }
                crate::encode::save_sdr_as_jxl_hdr(
                    t.w as u32,
                    t.h as u32,
                    &buf,
                    self.alpha.is_some(),
                    path,
                    crate::encode::QualityLevel::VeryHigh,
                    depth,
                    enhance,
                    ai_nits,
                )
                .map_err(|e| format!("JXL 保存失败: {}", e))
            }
        }
    }
}

/// 完整放大（TTA 可选）
pub fn upscale_image_full(
    model_root: &std::path::Path,
    kind: ModelKind,
    input: &image::RgbImage,
    iterations: usize,
    noise_level: Option<u32>,
    use_tta: bool,
) -> Result<(image::RgbImage, bool), String> {
    let model_file = match noise_level {
        Some(n) if kind.has_noise_scale_model() => {
            let f = kind.noise_scale_file(n);
            let p = model_root.join(kind.dir_name()).join(&f);
            if p.exists() {
                f
            } else {
                kind.scale_file().to_string()
            }
        }
        _ => kind.scale_file().to_string(),
    };
    let model = WaifuModel::load(kind, model_root, &model_file)?;
    let mut t = image_to_tensor(input);
    let mut used_gpu = false;

    for i in 0..iterations {
        if use_tta {
            t = run_once_tta(&model, &t)?;
            used_gpu = true;
        } else {
            match run_once_gpu(&model, &t) {
                Ok((out, _)) => {
                    used_gpu = true;
                    t = out;
                }
                Err(e) => {
                    if i == 0 {
                        log::info!("[upscale] GPU 不可用回落 CPU: {}", e);
                    }
                    t = run_once(&model, &t)?;
                }
            }
        }
    }
    Ok((tensor_to_image(&t), used_gpu))
}

#[cfg(test)]
mod golden_tests {
    use super::*;

    /// 新功能验证：alpha 保留 / 任意倍率 / 宽高换算 / 多格式输出
    #[test]
    fn test_rgba_scale_and_formats() {
        let root = model_root();

        // 1. calc_scale_ratio（原版 CalcScaleRatio 语义：双指定取 max 覆盖）
        assert_eq!(calc_scale_ratio(Some(2.5), None, None, 100, 50), 2.5);
        assert_eq!(calc_scale_ratio(None, Some(200), None, 100, 50), 2.0);
        assert_eq!(calc_scale_ratio(None, None, Some(100), 100, 50), 2.0);
        // 双指定：w=2x, h=3x → 取 3.0（覆盖式）
        assert_eq!(calc_scale_ratio(None, Some(200), Some(150), 100, 50), 3.0);

        // 2. 半透明 RGBA 输入 → alpha 保留（放大路径）
        let mut img = image::RgbaImage::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                img.put_pixel(
                    x,
                    y,
                    image::Rgba([
                        ((x * 7 + y * 3) % 256) as u8,
                        ((x * 5 + y * 11) % 256) as u8,
                        ((x * 13 + y * 2) % 256) as u8,
                        if x < 16 { 128 } else { 255 },
                    ]),
                );
            }
        }
        let (out, _) = upscale_image_exact(
            &root,
            ModelKind::Upconv7AnimeStyleArtRgb,
            &img,
            2.0,
            None,
            false,
        )
        .unwrap();
        assert_eq!((out.rgb.w, out.rgb.h), (64, 64), "RGB 2x 尺寸");
        let alpha = out.alpha.as_ref().expect("半透明输入应保留 alpha");
        assert_eq!((alpha.w, alpha.h), (64, 64), "alpha 2x 尺寸");
        // 左半（源 alpha=0.5）应显著小于右半（1.0）
        let (mut l, mut r) = (0.0f32, f32::INFINITY);
        for y in 0..64 {
            for x in 0..64 {
                let v = alpha.at(0, y, x);
                if x < 32 {
                    l = l.max(v);
                } else {
                    r = r.min(v);
                }
            }
        }
        assert!(l < 0.75, "左半 alpha 应 ≈0.5（max {l}）");
        assert!(r > 0.9, "右半 alpha 应 ≈1.0（min {r}）");

        // 3. 全不透明输入 → alpha None（快路径）
        let mut solid = img.clone();
        for p in solid.pixels_mut() {
            p.0[3] = 255;
        }
        let (out2, _) = upscale_image_exact(
            &root,
            ModelKind::Upconv7AnimeStyleArtRgb,
            &solid,
            2.0,
            None,
            false,
        )
        .unwrap();
        assert!(out2.alpha.is_none(), "全不透明输入不应走 alpha 路径");

        // 4. 任意倍率 3.0（ceil(log2 3)=2 轮 + 回缩）与 16x（4 轮）
        let (out3, _) = upscale_image_exact(
            &root,
            ModelKind::Upconv7AnimeStyleArtRgb,
            &solid,
            3.0,
            None,
            false,
        )
        .unwrap();
        assert_eq!((out3.rgb.w, out3.rgb.h), (96, 96), "3x 精确回缩");
        // 16x 小图（8×8 → 128×128）
        let tiny = image::RgbaImage::from_fn(8, 8, |x, y| {
            image::Rgba([(x * 31 % 256) as u8, (y * 31 % 256) as u8, 128, 255])
        });
        let (out16, _) = upscale_image_exact(
            &root,
            ModelKind::Upconv7AnimeStyleArtRgb,
            &tiny,
            16.0,
            None,
            false,
        )
        .unwrap();
        assert_eq!((out16.rgb.w, out16.rgb.h), (128, 128), "16x（4 轮迭代）");

        // 5. 纯降噪 alpha 不变（原样保留）
        let (dn, _) = denoise_only(&root, ModelKind::Cunet, &img, 1, false).unwrap();
        assert_eq!((dn.rgb.w, dn.rgb.h), (32, 32), "纯降噪尺寸不变");
        let dalpha = dn.alpha.as_ref().expect("纯降噪应保留 alpha 原样");
        assert!(
            (dalpha.at(0, 0, 0) - 128.0 / 255.0).abs() < 1e-4,
            "alpha 未过网络"
        );

        // 6. 多格式保存冒烟（PNG8/16、HDR PNG、EXR、JXL 12/16bit）
        let tmp = std::env::temp_dir().join("jietu-waifu2x-fmt");
        std::fs::create_dir_all(&tmp).unwrap();
        let p8 = tmp.join("a8.png");
        out.save(&p8, OutputFormat::Png8).unwrap();
        assert_eq!(image::open(&p8).unwrap().to_rgba8().dimensions(), (64, 64));
        let p16 = tmp.join("a16.png");
        out.save(&p16, OutputFormat::Png16).unwrap();
        assert_eq!(
            image::open(&p16).unwrap().to_rgba16().dimensions(),
            (64, 64)
        );
        // HDR PNG（16bit PQ + cICP）：能被 png 解码器读回（RGBA 16bit）
        let ph = tmp.join("a_hdr.png");
        out.save(&ph, OutputFormat::PngHdr).unwrap();
        assert_eq!(image::open(&ph).unwrap().to_rgba16().dimensions(), (64, 64));
        // EXR（32bit 浮点）
        let pe = tmp.join("a.exr");
        out.save(&pe, OutputFormat::Exr).unwrap();
        assert!(std::fs::metadata(&pe).unwrap().len() > 100, "EXR 输出非空");
        let pj16 = tmp.join("a16.jxl");
        out.save(&pj16, OutputFormat::JxlHdr16).unwrap();
        let sz16 = std::fs::metadata(&pj16).unwrap().len();
        assert!(sz16 > 100, "JXL 16bit 输出非空（{} B）", sz16);
        let pj12 = tmp.join("a12.jxl");
        out.save(&pj12, OutputFormat::JxlHdr12).unwrap();
        let sz12 = std::fs::metadata(&pj12).unwrap().len();
        assert!(sz12 > 100, "JXL 12bit 输出非空（{} B）", sz12);
        // 12bit 解码回读：非黑（FROM_PIXEL_FORMAT 量程 bug 回归——黑图仅 ~2.5KB 且 scRGB≈0）
        assert!(
            sz12 > 1000,
            "JXL 12bit 应有实际内容（{} B，纯黑仅 ~2.5KB）",
            sz12
        );
        assert_jxl_not_black(&pj12, "JXL 12bit");
        assert_jxl_not_black(&pj16, "JXL 16bit");
        eprintln!(
            "[fmt] png8/png16/pngHdr/exr/jxl12/jxl16 保存 OK（jxl16 {sz16} B / jxl12 {sz12} B）"
        );
    }

    /// JXL 解码回读防黑断言：色调映射 SDR 临时 PNG 最大通道值应 > 100
    ///
    /// 回归：libjxl 编码器默认 FROM_PIXEL_FORMAT 量程（Uint16=65535）曾把
    /// 12bit 输入（max_val=4095 量化）整体缩小 16 倍 → 纯黑文件（~2.5KB）
    fn assert_jxl_not_black(path: &std::path::Path, tag: &str) {
        let (temp, _w, _h) = crate::viewer::decode::decode_to_temp_png(path)
            .unwrap_or_else(|e| panic!("{} 解码失败: {}", tag, e));
        let img = image::open(&temp).unwrap().to_rgba8();
        let max = img
            .pixels()
            .map(|p| p.0[0].max(p.0[1]).max(p.0[2]))
            .max()
            .unwrap_or(0);
        assert!(max > 100, "{} 解码回读应为非黑图像（max={}）", tag, max);
    }

    fn model_root() -> std::path::PathBuf {
        std::path::PathBuf::from(r"e:\jietu\图片放大器\models")
    }

    /// tres1 手动逐步追踪：每层 ch0/ch1/ch2 角落值（无释放版 forward）
    #[test]
    fn test_tres1_trace() {
        let dir = std::path::PathBuf::from(r"e:\jietu\unused\probe-models\uprobe_tres1");
        if !dir.exists() {
            eprintln!("[trace] 探针未生成，跳过");
            return;
        }
        let net =
            crate::upscale::caffemodel::load_caffemodel(&dir.join("noise1_model.json.caffemodel"))
                .unwrap();
        let model = crate::upscale::model::WaifuModel::build(
            crate::upscale::model::ModelKind::Upresnet10,
            &net,
        )
        .unwrap();
        // 渐变输入（与 bisect 相同）
        let mut img = image::RgbImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                img.put_pixel(
                    x,
                    y,
                    image::Rgb([
                        ((x * 7 + y * 3) % 256) as u8,
                        ((x * 5 + y * 11) % 256) as u8,
                        ((x * 13 + y * 2) % 256) as u8,
                    ]),
                );
            }
        }
        let t = image_to_tensor(&img);
        const CROP: usize = 128;
        let pw = t.w.div_ceil(CROP) * CROP;
        let ph = t.h.div_ceil(CROP) * CROP;
        let mut full = Tensor::new(t.c, ph, pw);
        for c2 in 0..t.c {
            for y in 0..ph {
                let sy = y.min(t.h - 1);
                for x in 0..pw {
                    let sx = x.min(t.w - 1);
                    full.set(c2, y, x, t.at(c2, sy, sx));
                }
            }
        }
        let padded = pad_replicate(&full, 3);

        // 手动逐步（无释放）
        let mut blobs: Vec<Option<Tensor>> = vec![None; model.n_slots];
        blobs[0] = Some(padded);
        let mut last_use = vec![0usize; model.n_slots];
        for (i, l) in model.layers.iter().enumerate() {
            for b in l.bottoms() {
                last_use[b] = last_use[b].max(i);
            }
        }
        for (i, layer) in model.layers.iter().enumerate() {
            fn get<'a>(blobs: &'a [Option<Tensor>], s: usize) -> &'a Tensor {
                blobs[s].as_ref().unwrap()
            }
            let out = match layer {
                crate::upscale::model::ExecLayer::Conv {
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
                    let inp = get(&blobs, *bottom);
                    let mut o = Tensor::new(
                        *out_ch,
                        (inp.h + 2 * pad).saturating_sub((kernel - 1) * dilation + 1) / stride + 1,
                        (inp.w + 2 * pad).saturating_sub((kernel - 1) * dilation + 1) / stride + 1,
                    );
                    for oc in 0..*out_ch {
                        for y in 0..o.h {
                            for x in 0..o.w {
                                let mut acc = bias[oc];
                                for ic in 0..inp.c {
                                    for ky in 0..*kernel {
                                        let sy =
                                            (y * stride + ky * dilation) as isize - *pad as isize;
                                        for kx in 0..*kernel {
                                            let sx = (x * stride + kx * dilation) as isize
                                                - *pad as isize;
                                            let yy = sy.clamp(0, inp.h as isize - 1) as usize;
                                            let xx = sx.clamp(0, inp.w as isize - 1) as usize;
                                            acc += weight[(oc * inp.c + ic) * kernel * kernel
                                                + ky * kernel
                                                + kx]
                                                * inp.at(ic, yy, xx);
                                        }
                                    }
                                }
                                o.set(oc, y, x, if acc >= 0.0 { acc } else { acc * leaky_slope });
                            }
                        }
                    }
                    o
                }
                crate::upscale::model::ExecLayer::Deconv { .. } => unreachable!(),
                crate::upscale::model::ExecLayer::GlobalAvgPool { bottom, .. } => {
                    let inp = get(&blobs, *bottom);
                    let mut o = Tensor::new(inp.c, 1, 1);
                    for c in 0..inp.c {
                        let s: f32 = inp.data[c * inp.h * inp.w..(c + 1) * inp.h * inp.w]
                            .iter()
                            .sum();
                        o.data[c] = s / (inp.h * inp.w) as f32;
                    }
                    o
                }
                crate::upscale::model::ExecLayer::Sigmoid { bottom, .. } => {
                    let inp = get(&blobs, *bottom);
                    let mut o = Tensor::new(inp.c, inp.h, inp.w);
                    for (a, b) in o.data.iter_mut().zip(inp.data.iter()) {
                        *a = 1.0 / (1.0 + (-*b).exp());
                    }
                    o
                }
                crate::upscale::model::ExecLayer::Scale { .. } => unreachable!(),
                crate::upscale::model::ExecLayer::Axpy { alpha, x, y, .. } => {
                    let a = get(&blobs, *alpha);
                    let xi = get(&blobs, *x);
                    let yi = get(&blobs, *y);
                    let mut o = Tensor::new(xi.c, xi.h, xi.w);
                    for c in 0..xi.c {
                        let av = a.data.get(c).copied().unwrap_or(a.data[0]);
                        for k in 0..xi.h * xi.w {
                            o.data[c * xi.h * xi.w + k] =
                                av * xi.data[c * xi.h * xi.w + k] + yi.data[c * xi.h * xi.w + k];
                        }
                    }
                    eprintln!(
                        "[trace] axpy 输入: alpha[0..3]={:?}",
                        &a.data[..3.min(a.data.len())]
                    );
                    eprintln!(
                        "[trace]   x ch0 角={:.4} y ch0 角={:.4} → out ch0 角={:.4}",
                        xi.data[0], yi.data[0], o.data[0]
                    );
                    o
                }
                crate::upscale::model::ExecLayer::CropCenter {
                    crop_h,
                    crop_w,
                    bottom,
                    ..
                } => {
                    let inp = get(&blobs, *bottom);
                    let mut o = Tensor::new(inp.c, inp.h - 2 * crop_h, inp.w - 2 * crop_w);
                    for c in 0..inp.c {
                        for yy in 0..o.h {
                            let src = (c * inp.h + yy + crop_h) * inp.w + crop_w;
                            let dst = (c * o.h + yy) * o.w;
                            o.data[dst..dst + o.w].copy_from_slice(&inp.data[src..src + o.w]);
                        }
                    }
                    eprintln!("[trace] crop 角 ch0={:.4}", o.data[0]);
                    o
                }
                crate::upscale::model::ExecLayer::EltwiseAdd { .. } => unreachable!(),
                crate::upscale::model::ExecLayer::Crop { .. } => unreachable!(),
                crate::upscale::model::ExecLayer::Concat { .. } => unreachable!(),
                crate::upscale::model::ExecLayer::NearestUp2 { .. } => unreachable!(),
                crate::upscale::model::ExecLayer::ScaledAdd { .. } => unreachable!(),
            };
            let kind = match layer {
                crate::upscale::model::ExecLayer::Conv { .. } => "Conv",
                crate::upscale::model::ExecLayer::Deconv { .. } => "Deconv",
                crate::upscale::model::ExecLayer::GlobalAvgPool { .. } => "GAP",
                crate::upscale::model::ExecLayer::Sigmoid { .. } => "Sigmoid",
                crate::upscale::model::ExecLayer::Scale { .. } => "Scale",
                crate::upscale::model::ExecLayer::Axpy { .. } => "Axpy",
                crate::upscale::model::ExecLayer::CropCenter { .. } => "Crop",
                crate::upscale::model::ExecLayer::EltwiseAdd { .. } => "Add",
                _ => "?",
            };
            let c0 = out.data[0];
            eprintln!(
                "[trace] 层{i:>2} {kind} ch0 角={c0:.4} ({}x{}x{})",
                out.c, out.h, out.w
            );
            blobs[layer.top()] = Some(out);
            let _ = &mut blobs;
        }
    }

    fn cui_exe() -> std::path::PathBuf {
        std::path::PathBuf::from(r"e:\jietu\图片放大器\waifu2x-caffe-cui.exe")
    }

    /// Real-ESRGAN 黄金对拍：原版 realesrgan-ncnn-vulkan 参考输出 vs 本实现
    ///
    /// 输入 grad32.png（32×32 确定性渐变，与 ncnn 参考生成同源）；参考图
    /// ref_x4.png / ref_anime.png（×4 → 128×128）。ncnn 后处理 clip [0,1]×255。
    #[test]
    fn test_golden_vs_realesrgan_ncnn() {
        let dir = std::path::PathBuf::from(r"e:\jietu\unused\realesrgan-ncnn");
        let in_path = dir.join("grad96.png");
        if !in_path.exists() || !dir.join("ref96_x4.png").exists() {
            eprintln!("[golden-re] 参考图缺失，跳过（先跑 ncnn 生成）");
            return;
        }
        let root = model_root();
        let img = image::open(&in_path).unwrap().to_rgba8();
        let (iw, ih) = (img.width() as usize, img.height() as usize);
        let cases = [
            (ModelKind::RealEsrganX4, "ref96_x4.png"),
            (ModelKind::RealEsrganAnime6B, "ref96_anime.png"),
        ];
        for (kind, ref_name) in cases {
            let f = kind.scale_file();
            let exists = root.join(&f).exists() || root.join("RealESRGAN").join(&f).exists();
            if !exists {
                eprintln!("[golden-re] {} 缺失，跳过", f);
                continue;
            }
            let model = WaifuModel::load(kind, &root, kind.scale_file()).unwrap();
            // ncnn 前处理同源：RGB /255 → 前向 → clip ×255
            let (rgb, _) = rgba_to_tensors(&img);
            let out = run_once(&model, &rgb).unwrap();
            let (mut mse, mut max_lsb, mut cnt) = (0.0f64, 0i32, 0usize);
            let reference = image::open(dir.join(ref_name)).unwrap().to_rgba8();
            assert_eq!(reference.dimensions(), (384, 384), "{} 参考图尺寸", f);
            // 只对比中心区（去 80px 输出边界 = 20px 输入边界）：ncnn tile 边界用
            // replicate pad（PyTorch 语义为 zero pad），边界圈属预期实现差异
            for y in 80..304 {
                for x in 80..304 {
                    for c in 0..3 {
                        let v = (out.at(c, y, x).clamp(0.0, 1.0) * 255.0 + 0.5) as i32;
                        let r = reference.get_pixel(x as u32, y as u32).0[c] as i32;
                        let d = (v - r).abs();
                        max_lsb = max_lsb.max(d);
                        mse += (d * d) as f64;
                        cnt += 1;
                    }
                }
            }
            let psnr = 10.0 * (255.0 * 255.0 / (mse / cnt as f64)).log10();
            let _ = (iw, ih);
            eprintln!(
                "[golden-re] {}（中心区）: max={max_lsb} LSB PSNR={psnr:.2}dB",
                f
            );
            // ncnn 权重为 fp16 量化 + 深网（23 块）误差累积 → 中心区 PSNR 应显著高
            assert!(
                psnr > 35.0,
                "{} 与 ncnn 参考差异过大: PSNR={psnr:.2}dB max={max_lsb}",
                f
            );
        }
    }

    fn max_lsb_of(a: &image::RgbImage, b: &image::RgbImage) -> i32 {
        let mut m = 0i32;
        for (p, q) in a.pixels().zip(b.pixels()) {
            for i in 0..3 {
                m = m.max((p.0[i] as i32 - q.0[i] as i32).abs());
            }
        }
        m
    }

    /// 探针执行：已知权重探针模型（unused/probe-models/uprobe）跑原版 cui，
    /// 输出像素对照假设表 → 测定 Axpy/Scale/InnerProduct 精确语义
    #[test]
    fn test_run_semantics_probe() {
        let probe_dir = std::path::PathBuf::from(r"e:\jietu\unused\probe-models\uprobe");
        if !probe_dir.exists() {
            eprintln!("[probe] 探针模型未生成（node gen-probe-model.js），跳过");
            return;
        }
        // 输入图 64×64 RGB(26,38,51) = v=[0.1,0.15,0.2]
        let mut img = image::RgbImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                img.put_pixel(x, y, image::Rgb([26, 38, 51]));
            }
        }
        let tmp = std::path::PathBuf::from(r"e:\jietu\unused\probe-run");
        std::fs::create_dir_all(&tmp).unwrap();
        let in_path = tmp.join("in.png");
        img.save(&in_path).unwrap();
        let out_path = tmp.join("probe_out.png");
        let _ = std::fs::remove_file(&out_path);

        let out = std::process::Command::new(cui_exe())
            .current_dir(cui_exe().parent().unwrap())
            .args([
                "-m",
                "noise",
                "-n",
                "1",
                "-p",
                "cpu",
                "--model_dir",
                &probe_dir.to_string_lossy(),
                "-i",
                &in_path.to_string_lossy(),
                "-o",
                &out_path.to_string_lossy(),
            ])
            .output()
            .expect("运行 cui 失败");
        assert!(
            out.status.success(),
            "cui 探针失败: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let result = image::open(&out_path).expect("读探针输出失败").to_rgb8();
        let c = result.get_pixel(result.width() / 2, result.height() / 2).0;
        eprintln!(
            "[probe] 原版输出中心像素: [{}, {}, {}]（尺寸 {}x{}）",
            c[0],
            c[1],
            c[2],
            result.width(),
            result.height()
        );
        let c2 = result.get_pixel(2, 2).0;
        eprintln!(
            "[probe] 原版输出角内像素: [{}, {}, {}]",
            c2[0], c2[1], c2[2]
        );

        // 本实现跑同一探针（offset=0 无 padding；直接 forward）
        let net = crate::upscale::caffemodel::load_caffemodel(
            &probe_dir.join("noise1_model.json.caffemodel"),
        )
        .unwrap();
        let model = crate::upscale::model::WaifuModel::build(
            crate::upscale::model::ModelKind::Upresnet10,
            &net,
        )
        .unwrap();
        let inp = image_to_tensor(&img);
        let out = cpu::forward(&model, &inp).unwrap();
        let mine = tensor_to_image(&out);
        let m = mine.get_pixel(mine.width() / 2, mine.height() / 2).0;
        eprintln!(
            "[probe] 本实现输出中心像素: [{}, {}, {}]（尺寸 {}x{}）",
            m[0],
            m[1],
            m[2],
            mine.width(),
            mine.height()
        );
        // 期望（node 表）：fc[out,in] + a*x+y + *v ≈ [37,58,77]
        eprintln!("[probe] 判定: 原版={:?} 本实现={:?}", c, m);
    }

    /// 截断二分：真实 upresnet10 前 N 层探针（真实权重）→ 定位首个分歧层
    #[test]
    fn test_truncation_bisect() {
        let root = std::path::PathBuf::from(r"e:\jietu\unused\probe-models");
        let tmp = std::path::PathBuf::from(r"e:\jietu\unused\probe-run");
        std::fs::create_dir_all(&tmp).unwrap();
        // 非纯色输入（渐变，避免平坦退化）
        let mut img = image::RgbImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                img.put_pixel(
                    x,
                    y,
                    image::Rgb([
                        ((x * 7 + y * 3) % 256) as u8,
                        ((x * 5 + y * 11) % 256) as u8,
                        ((x * 13 + y * 2) % 256) as u8,
                    ]),
                );
            }
        }
        let in_path = tmp.join("in_grad.png");
        img.save(&in_path).unwrap();

        let mut first_diverge: Option<usize> = None;
        for n in 2..=63usize {
            let dir = root.join(format!("uprobe_ttrunc_{n:02}"));
            if !dir.exists() {
                continue;
            }
            // 本实现先跑：净输出 < 64（GAP/裁剪尾层）或构建失败 → 原版基准不可靠，跳过
            let net = match crate::upscale::caffemodel::load_caffemodel(
                &dir.join("noise1_model.json.caffemodel"),
            ) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let info: serde_json::Value = match serde_json::from_str(
                &std::fs::read_to_string(dir.join("info.json")).unwrap(),
            ) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let offset = info["offset"].as_u64().unwrap() as usize;
            let model = match crate::upscale::model::WaifuModel::build(
                crate::upscale::model::ModelKind::Upresnet10,
                &net,
            ) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let t = image_to_tensor(&img);
            const CROP: usize = 128;
            let pw = t.w.div_ceil(CROP) * CROP;
            let ph = t.h.div_ceil(CROP) * CROP;
            let mut full = Tensor::new(t.c, ph, pw);
            for c2 in 0..t.c {
                for y in 0..ph {
                    let sy = y.min(t.h - 1);
                    for x in 0..pw {
                        let sx = x.min(t.w - 1);
                        full.set(c2, y, x, t.at(c2, sy, sx));
                    }
                }
            }
            let padded = pad_replicate(&full, offset);
            let net_out = match cpu::forward(&model, &padded) {
                Ok(o) => o,
                Err(_) => continue,
            };
            if net_out.h < 64 || net_out.w < 64 {
                continue; // 尾层为 GAP 等尺寸坍缩 → 原版基准无效
            }

            let out_path = tmp.join(format!("trunc_{n:02}.png"));
            let _ = std::fs::remove_file(&out_path);
            let ok = std::process::Command::new(cui_exe())
                .current_dir(cui_exe().parent().unwrap())
                .args([
                    "-m",
                    "noise",
                    "-n",
                    "1",
                    "-p",
                    "cpu",
                    "--model_dir",
                    &dir.to_string_lossy(),
                    "-i",
                    &in_path.to_string_lossy(),
                    "-o",
                    &out_path.to_string_lossy(),
                ])
                .output();
            let Ok(out) = ok else { continue };
            if !out.status.success() {
                eprintln!("[bisect] N={n} cui 失败（跳过）");
                continue;
            }
            let reference = image::open(&out_path).unwrap().to_rgb8();

            // noise 模式（inner=1）原版取净输出左上 [0, size)
            let (oh, ow) = (64.min(net_out.h), 64.min(net_out.w));
            let mut cropped = Tensor::new(net_out.c, oh, ow);
            for c2 in 0..net_out.c {
                for y in 0..oh {
                    for x in 0..ow {
                        cropped.set(c2, y, x, net_out.at(c2, y, x));
                    }
                }
            }
            let mine = tensor_to_image(&cropped);
            // 调试：保存本实现输出
            let _ = mine.save(tmp.join(format!("mine_trunc_{n:02}.png")));

            // 比较（量化后）
            let (mut max_lsb, mut sum) = (0i32, 0i64);
            let mut cnt = 0usize;
            for (a, b) in reference.pixels().zip(mine.pixels()) {
                for i in 0..3 {
                    let d = (a.0[i] as i32 - b.0[i] as i32).abs();
                    max_lsb = max_lsb.max(d);
                    sum += d as i64;
                    cnt += 1;
                }
            }
            let mean = sum as f64 / cnt as f64;
            let mark = if max_lsb > 2 { " ← 分歧!" } else { "" };
            eprintln!("[bisect] N={n:>2}: max={max_lsb:>3} LSB mean={mean:.3}{mark}");
            if n == 5 {
                // 调试：打印前 3 行像素对比
                for y in 0..3 {
                    let mut r = String::new();
                    let mut m = String::new();
                    for x in 0..8 {
                        let a = reference.get_pixel(x, y).0;
                        let b = mine.get_pixel(x, y).0;
                        r.push_str(&format!("({},{},{}) ", a[0], a[1], a[2]));
                        m.push_str(&format!("({},{},{}) ", b[0], b[1], b[2]));
                    }
                    eprintln!("[dbg] ref 行{y}: {r}");
                    eprintln!("[dbg] mine行{y}: {m}");
                }
            }
            if max_lsb > 2 && first_diverge.is_none() {
                first_diverge = Some(n);
            }
        }
        if let Some(n) = first_diverge {
            eprintln!("[bisect] 首个分歧截断: N={n}（即层 {} 引入分歧）", n - 1);
        } else {
            eprintln!("[bisect] 全部截断一致?!（原模型应为分歧）");
        }

        // 转置验证：uprobe_trunc_05t（conv1 权重数据已转置 [in][out]）
        // 若 原版(05t) == 本实现(05 原始) → 原版把方形 conv 权重按 [in][out] 读取
        {
            let dir = root.join("uprobe_trunc_05t");
            if dir.exists() {
                let out_path = tmp.join("trunc_05t.png");
                let _ = std::fs::remove_file(&out_path);
                let out = std::process::Command::new(cui_exe())
                    .current_dir(cui_exe().parent().unwrap())
                    .args([
                        "-m",
                        "noise",
                        "-n",
                        "1",
                        "-p",
                        "cpu",
                        "--model_dir",
                        &dir.to_string_lossy(),
                        "-i",
                        &tmp.join("in_grad.png").to_string_lossy(),
                        "-o",
                        &out_path.to_string_lossy(),
                    ])
                    .output()
                    .unwrap();
                if out.status.success() {
                    let reference_t = image::open(&out_path).unwrap().to_rgb8();
                    // 本实现跑 05t（转置版）
                    let net = crate::upscale::caffemodel::load_caffemodel(
                        &dir.join("noise1_model.json.caffemodel"),
                    )
                    .unwrap();
                    let model = crate::upscale::model::WaifuModel::build(
                        crate::upscale::model::ModelKind::Upresnet10,
                        &net,
                    )
                    .unwrap();
                    let t = image_to_tensor(&img);
                    const CROP: usize = 128;
                    let pw = t.w.div_ceil(CROP) * CROP;
                    let ph = t.h.div_ceil(CROP) * CROP;
                    let mut full = Tensor::new(t.c, ph, pw);
                    for c2 in 0..t.c {
                        for y in 0..ph {
                            let sy = y.min(t.h - 1);
                            for x in 0..pw {
                                let sx = x.min(t.w - 1);
                                full.set(c2, y, x, t.at(c2, sy, sx));
                            }
                        }
                    }
                    let padded = pad_replicate(&full, 2);
                    let net_out = cpu::forward(&model, &padded).unwrap();
                    let mut cropped = Tensor::new(net_out.c, 64, 64);
                    for c2 in 0..net_out.c {
                        for y in 0..64 {
                            for x in 0..64 {
                                cropped.set(c2, y, x, net_out.at(c2, y, x));
                            }
                        }
                    }
                    let mine_t = tensor_to_image(&cropped);

                    // 统计：原版(05t) vs 本实现(05t) 和 原版(05t) vs 本实现(05 原始)
                    let ref_t_vs_mine_t = max_lsb_of(&reference_t, &mine_t);
                    let old_ref = image::open(tmp.join("trunc_05.png")).unwrap().to_rgb8();
                    // 本实现(05 原始) 需重算——读取 mine_trunc_05.png
                    let mine_orig = image::open(tmp.join("mine_trunc_05.png"))
                        .unwrap()
                        .to_rgb8();
                    let ref_t_vs_mine_orig = max_lsb_of(&reference_t, &mine_orig);
                    let ref_05_vs_mine_orig = max_lsb_of(&old_ref, &mine_orig);
                    eprintln!(
                        "[transpose] 原版(05t) vs 本实现(05t): {} LSB；原版(05t) vs 本实现(05原始): {} LSB；原版(05) vs 本实现(05原始): {} LSB",
                        ref_t_vs_mine_t, ref_t_vs_mine_orig, ref_05_vs_mine_orig
                    );
                    eprintln!(
                        "[transpose] 判定: 若『原版(05t) vs 本实现(05原始)』显著小于其余 → 原版按 [in][out] 读方形 conv 权重"
                    );
                }
            }
        }

        // 通道验证（文本 prototxt 可靠版）：outproj 选 conv_pre 的不同通道段
        for base in [0usize, 3, 8, 30, 60] {
            let dir = root.join(format!("uprobe_chant_{base:02}"));
            if !dir.exists() {
                continue;
            }
            let out_path = tmp.join(format!("chan_{base:02}.png"));
            let _ = std::fs::remove_file(&out_path);
            let out = std::process::Command::new(cui_exe())
                .current_dir(cui_exe().parent().unwrap())
                .args([
                    "-m",
                    "noise",
                    "-n",
                    "1",
                    "-p",
                    "cpu",
                    "--model_dir",
                    &dir.to_string_lossy(),
                    "-i",
                    &tmp.join("in_grad.png").to_string_lossy(),
                    "-o",
                    &out_path.to_string_lossy(),
                ])
                .output()
                .unwrap();
            if !out.status.success() {
                eprintln!("[chan] base={base} cui 失败");
                continue;
            }
            let reference = image::open(&out_path).unwrap().to_rgb8();
            let net = crate::upscale::caffemodel::load_caffemodel(
                &dir.join("noise1_model.json.caffemodel"),
            )
            .unwrap();
            let model = crate::upscale::model::WaifuModel::build(
                crate::upscale::model::ModelKind::Upresnet10,
                &net,
            )
            .unwrap();
            let t = image_to_tensor(&img);
            const CROP: usize = 128;
            let pw = t.w.div_ceil(CROP) * CROP;
            let ph = t.h.div_ceil(CROP) * CROP;
            let mut full = Tensor::new(t.c, ph, pw);
            for c2 in 0..t.c {
                for y in 0..ph {
                    let sy = y.min(t.h - 1);
                    for x in 0..pw {
                        let sx = x.min(t.w - 1);
                        full.set(c2, y, x, t.at(c2, sy, sx));
                    }
                }
            }
            let padded = pad_replicate(&full, 1);
            let net_out = cpu::forward(&model, &padded).unwrap();
            let mut cropped = Tensor::new(net_out.c, 64, 64);
            for c2 in 0..net_out.c {
                for y in 0..64 {
                    for x in 0..64 {
                        cropped.set(c2, y, x, net_out.at(c2, y, x));
                    }
                }
            }
            let mine = tensor_to_image(&cropped);
            let m = max_lsb_of(&reference, &mine);
            let rc = reference.get_pixel(0, 0).0;
            let mc = mine.get_pixel(0, 0).0;
            eprintln!(
                "[chan] conv_pre ch {base}..{}: max={m} LSB {}；角落 ref={rc:?} mine={mc:?}",
                base + 2,
                if m > 2 { "← 分歧!" } else { "✓" },
            );
        }

        // 定向探针：tcrop（CropCenter）/ tse（SE 链+Axpy）/ tres1（完整 res1 块）
        for name in ["tcrop", "tse", "tres1"] {
            let dir = root.join(format!("uprobe_{name}"));
            if !dir.exists() {
                continue;
            }
            let out_path = tmp.join(format!("tgt_{name}.png"));
            let _ = std::fs::remove_file(&out_path);
            let out = std::process::Command::new(cui_exe())
                .current_dir(cui_exe().parent().unwrap())
                .args([
                    "-m",
                    "noise",
                    "-n",
                    "1",
                    "-p",
                    "cpu",
                    "--model_dir",
                    &dir.to_string_lossy(),
                    "-i",
                    &tmp.join("in_grad.png").to_string_lossy(),
                    "-o",
                    &out_path.to_string_lossy(),
                ])
                .output()
                .unwrap();
            if !out.status.success() {
                eprintln!("[tgt] {name} cui 失败");
                continue;
            }
            let reference = image::open(&out_path).unwrap().to_rgb8();
            let net = crate::upscale::caffemodel::load_caffemodel(
                &dir.join("noise1_model.json.caffemodel"),
            )
            .unwrap();
            let model = crate::upscale::model::WaifuModel::build(
                crate::upscale::model::ModelKind::Upresnet10,
                &net,
            )
            .unwrap();
            let t = image_to_tensor(&img);
            const CROP: usize = 128;
            let pw = t.w.div_ceil(CROP) * CROP;
            let ph = t.h.div_ceil(CROP) * CROP;
            let mut full = Tensor::new(t.c, ph, pw);
            for c2 in 0..t.c {
                for y in 0..ph {
                    let sy = y.min(t.h - 1);
                    for x in 0..pw {
                        let sx = x.min(t.w - 1);
                        full.set(c2, y, x, t.at(c2, sy, sx));
                    }
                }
            }
            let padded = pad_replicate(&full, 3);
            let net_out = cpu::forward(&model, &padded).unwrap();
            let mut cropped = Tensor::new(net_out.c, 64.min(net_out.h), 64.min(net_out.w));
            for c2 in 0..net_out.c {
                for y in 0..cropped.h {
                    for x in 0..cropped.w {
                        cropped.set(c2, y, x, net_out.at(c2, y, x));
                    }
                }
            }
            let mine = tensor_to_image(&cropped);
            let m = max_lsb_of(&reference, &mine);
            let rc = reference.get_pixel(0, 0).0;
            let mc = mine.get_pixel(0, 0).0;
            eprintln!("[tgt] {name}: max={m} LSB；角落 ref={rc:?} mine={mc:?}",);
            if name == "tres1" {
                // 打印 axpy 三输入的角落值（用 slot_shapes 找层）
                for (li, layer) in model.layers.iter().enumerate() {
                    if let crate::upscale::model::ExecLayer::Axpy { alpha, x, y, .. } = layer {
                        // 重跑 forward 并保留所有 blob（不复用释放版 forward——手动重算关键槽位）
                        // 简化：直接对 padded 再跑一次带跟踪的版本不可行——用特征值推断：
                        // alpha = sigmoid(fc2(relu(fc1(GAP(conv2_relu))))) — 由 tse 已验证
                        // 此处打印 conv2_relu 与 crop 的角落 ch0 值：重算前几层
                        let _ = (alpha, x, y);
                        eprintln!("[tgt] axpy 层索引 {li}（详见下方手工追踪）");
                    }
                }
            }
        }
    }

    /// 探针 v2/v3：upresnet10 专属算子（非原地 ReLU/Split/Eltwise/CropCenter）
    /// 与 cunet 专属算子（k2s2 conv/deconv/参考 Crop/奇数尺寸链）
    #[test]
    fn test_run_semantics_probe_v2v3() {
        let cases = [
            // (探针目录, 期望输出, 说明)
            (
                r"e:\jietu\unused\probe-models\uprobe2",
                [10, 15, 20],
                "upresnet10 算子链",
            ),
            (
                r"e:\jietu\unused\probe-models\uprobe3",
                [51, 77, 102],
                "cunet 算子链",
            ),
        ];
        let tmp = std::path::PathBuf::from(r"e:\jietu\unused\probe-run");
        std::fs::create_dir_all(&tmp).unwrap();
        let mut img = image::RgbImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                img.put_pixel(x, y, image::Rgb([26, 38, 51]));
            }
        }
        let in_path = tmp.join("in.png");
        img.save(&in_path).unwrap();

        for (dir, expect, desc) in cases {
            let probe_dir = std::path::PathBuf::from(dir);
            if !probe_dir.exists() {
                eprintln!("[probe] {desc} 未生成，跳过");
                continue;
            }
            let out_path = tmp.join(format!(
                "out_{}.png",
                probe_dir.file_name().unwrap().to_string_lossy()
            ));
            let _ = std::fs::remove_file(&out_path);
            let out = std::process::Command::new(cui_exe())
                .current_dir(cui_exe().parent().unwrap())
                .args([
                    "-m",
                    "noise",
                    "-n",
                    "1",
                    "-p",
                    "cpu",
                    "--model_dir",
                    &probe_dir.to_string_lossy(),
                    "-i",
                    &in_path.to_string_lossy(),
                    "-o",
                    &out_path.to_string_lossy(),
                ])
                .output()
                .expect("运行 cui 失败");
            assert!(
                out.status.success(),
                "{desc} cui 失败: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let result = image::open(&out_path).unwrap().to_rgb8();
            let c = result.get_pixel(result.width() / 2, result.height() / 2).0;
            eprintln!(
                "[probe] {desc} 原版输出: [{}, {}, {}]（期望 {:?}）",
                c[0], c[1], c[2], expect
            );

            // 本实现（读 info.json offset → 复现原版 padding 语义：crop 128 倍数 + offset）
            let net = crate::upscale::caffemodel::load_caffemodel(
                &probe_dir.join("noise1_model.json.caffemodel"),
            )
            .unwrap();
            let info: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(probe_dir.join("info.json")).unwrap(),
            )
            .unwrap();
            let offset = info["offset"].as_u64().unwrap() as usize;
            let model = crate::upscale::model::WaifuModel::build(
                crate::upscale::model::ModelKind::Upresnet10,
                &net,
            )
            .unwrap();
            let t = image_to_tensor(&img);
            const CROP: usize = 128;
            let pw = t.w.div_ceil(CROP) * CROP;
            let ph = t.h.div_ceil(CROP) * CROP;
            let mut full = Tensor::new(t.c, ph, pw);
            for c2 in 0..t.c {
                for y in 0..ph {
                    let sy = y.min(t.h - 1);
                    for x in 0..pw {
                        let sx = x.min(t.w - 1);
                        full.set(c2, y, x, t.at(c2, sy, sx));
                    }
                }
            }
            let padded = pad_replicate(&full, offset);
            let net_out = cpu::forward(&model, &padded).unwrap();
            let mut cropped = Tensor::new(net_out.c, 64, 64);
            for c2 in 0..net_out.c {
                for y in 0..64 {
                    for x in 0..64 {
                        cropped.set(c2, y, x, net_out.at(c2, offset + y, offset + x));
                    }
                }
            }
            let mine = tensor_to_image(&cropped);
            let m = mine.get_pixel(32, 32).0;
            eprintln!("[probe] {desc} 本实现输出: [{}, {}, {}]", m[0], m[1], m[2]);
        }
    }

    /// 权重布局探针：cunet 的 k2s2p0 deconv 在纯色输入下两种布局 ([in,out] vs [out,in])
    /// 的输出 spread——正确布局应与 forward 实测 spread 同量级
    #[test]
    fn test_deconv_layout_probe() {
        let net = crate::upscale::caffemodel::load_caffemodel(
            &model_root()
                .join("cunet")
                .join("scale2.0x_model.json.caffemodel"),
        )
        .unwrap();
        const V: f32 = 0.5; // 纯色近似
        for l in &net.layers {
            if l.layer_type != "Deconvolution" {
                continue;
            }
            let w = &l.blobs[0];
            let bias = &l.blobs[1].data;
            let (a, b, k) = (w.shape[0], w.shape[1], w.shape[2]);
            if k != 2 {
                continue; // 只探 k2s2
            }
            for (name, transposed) in [("[in,out]", false), ("[out,in]", true)] {
                // out_phase(oc, ky, kx) = Σ_ic V·w[idx] + b[oc]
                let mut vals: Vec<f32> = Vec::new();
                for oc in 0..b {
                    for ky in 0..k {
                        for kx in 0..k {
                            let mut acc = bias[oc];
                            for ic in 0..a {
                                let idx = if transposed {
                                    // 布局 [out,in,ky,kx]: w[oc, ic, ky, kx]
                                    (oc * a + ic) * k * k + ky * k + kx
                                } else {
                                    // 布局 [in,out,ky,kx]: w[ic, oc, ky, kx]
                                    (ic * b + oc) * k * k + ky * k + kx
                                };
                                acc += V * w.data[idx];
                            }
                            vals.push(acc);
                        }
                    }
                }
                let mn = vals.iter().copied().fold(f32::INFINITY, f32::min);
                let mx = vals.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                eprintln!(
                    "[probe] {} {} [{}x{}x{}x{}]: spread={:.4}",
                    l.name,
                    name,
                    w.shape[0],
                    w.shape[1],
                    w.shape[2],
                    w.shape[3],
                    mx - mn
                );
            }
        }
    }

    /// 黄金对拍：原版 waifu2x-caffe-cui（CPU，crop 128）vs 本实现 CPU 整图
    ///
    /// 复现原版 padding 语义（stImage::PaddingImage）：
    /// 输入 replicate-pad 到 crop 倍数（右/下）+ 每边 net_offset → forward
    /// → 中心裁剪 [offset, offset + 原尺寸×inner_scale)（= 原版 block 中心裁剪
    ///  + SetReconstructedImage 取原始尺寸区域的等价合成）
    #[test]
    fn test_golden_vs_original_cui() {
        const CROP: usize = 128;
        const SIZE: usize = 64;
        for kind in [
            ModelKind::Upconv7AnimeStyleArtRgb,
            ModelKind::Upresnet10,
            ModelKind::Cunet,
        ] {
            // 1. 确定性测试图（渐变，全面覆盖）
            let mut img = image::RgbImage::new(SIZE as u32, SIZE as u32);
            for y in 0..SIZE {
                for x in 0..SIZE {
                    img.put_pixel(
                        x as u32,
                        y as u32,
                        image::Rgb([
                            ((x * 7 + y * 3) % 256) as u8,
                            ((x * 5 + y * 11) % 256) as u8,
                            ((x * 13 + y * 2) % 256) as u8,
                        ]),
                    );
                }
            }
            let tmp = std::env::temp_dir().join("jietu-waifu2x-golden");
            std::fs::create_dir_all(&tmp).unwrap();
            let in_path = tmp.join("in.png");
            img.save(&in_path).unwrap();
            let ref_path = tmp.join(format!("ref_{}.png", kind.dir_name()));

            // 2. 原版 cui（CPU 整图单块：64 < crop 128 → SE 统计覆盖整 padded 图）
            let out = std::process::Command::new(cui_exe())
                .current_dir(cui_exe().parent().unwrap())
                .args([
                    "-m",
                    "scale",
                    "--scale",
                    "2",
                    "-p",
                    "cpu",
                    "--model_dir",
                    &model_root().join(kind.dir_name()).to_string_lossy(),
                    "-i",
                    &in_path.to_string_lossy(),
                    "-o",
                    &ref_path.to_string_lossy(),
                ])
                .output()
                .expect("运行原版 cui 失败");
            assert!(
                out.status.success(),
                "{} cui 失败: {}",
                kind.dir_name(),
                String::from_utf8_lossy(&out.stderr)
            );

            // 3. 本实现（CPU）：复现原版 padding 语义
            let m = WaifuModel::load(kind, &model_root(), kind.scale_file()).unwrap();
            let t = image_to_tensor(&img);
            // replicate-pad 到 crop 倍数（右/下）——原版 PaddingImage
            let pw = t.w.div_ceil(CROP) * CROP;
            let ph = t.h.div_ceil(CROP) * CROP;
            let mut full = Tensor::new(t.c, ph, pw);
            for c in 0..t.c {
                for y in 0..ph {
                    let sy = y.min(t.h - 1);
                    for x in 0..pw {
                        let sx = x.min(t.w - 1);
                        full.set(c, y, x, t.at(c, sy, sx));
                    }
                }
            }
            let padded = pad_replicate(&full, m.net_offset);
            let net_out = cpu::forward(&m, &padded).unwrap();
            // 中心裁剪 [offset, offset + SIZE×inner)
            let (ow, oh) = (SIZE * m.inner_scale, SIZE * m.inner_scale);
            let mut cropped = Tensor::new(net_out.c, oh, ow);
            for c in 0..net_out.c {
                for y in 0..oh {
                    for x in 0..ow {
                        cropped.set(c, y, x, net_out.at(c, m.net_offset + y, m.net_offset + x));
                    }
                }
            }
            let mine = tensor_to_image(&cropped);
            // 调试：保存本实现输出供外部比对
            let _ = mine.save(tmp.join(format!("mine_{}.png", kind.dir_name())));

            // 4. 比较（8bit 量化后逐通道 LSB 差）
            let reference = image::open(&ref_path)
                .unwrap_or_else(|e| panic!("读基准失败 {}: {}", ref_path.display(), e))
                .to_rgb8();
            // 调试：中心/角落像素对比
            {
                let (cw, ch) = (mine.width() / 2, mine.height() / 2);
                eprintln!(
                    "[dbg] {} 中心: ref={:?} mine={:?}；角落 ref={:?} mine={:?}",
                    kind.dir_name(),
                    reference.get_pixel(cw, ch).0,
                    mine.get_pixel(cw, ch).0,
                    reference.get_pixel(0, 0).0,
                    mine.get_pixel(0, 0).0
                );
            }
            assert_eq!(
                reference.dimensions(),
                mine.dimensions(),
                "{} 输出尺寸",
                kind.dir_name()
            );
            let mut max_lsb: i32 = 0;
            let mut over1: usize = 0;
            let mut total: usize = 0;
            for (a, b) in reference.pixels().zip(mine.pixels()) {
                for i in 0..3 {
                    let d = (a.0[i] as i32 - b.0[i] as i32).abs();
                    max_lsb = max_lsb.max(d);
                    if d > 1 {
                        over1 += 1;
                    }
                    total += 1;
                }
            }
            eprintln!(
                "[golden] {}: max={max_lsb} LSB, >1LSB {over1}/{total} ({:.3}%)",
                kind.dir_name(),
                over1 as f64 * 100.0 / total as f64
            );
            // 调试：错误行/列分布
            {
                let mut bad_rows = std::collections::BTreeMap::new();
                let mut bad_cols = std::collections::BTreeMap::new();
                for y in 0..reference.height() {
                    for x in 0..reference.width() {
                        let a = reference.get_pixel(x, y).0;
                        let b = mine.get_pixel(x, y).0;
                        let d: i32 = (0..3)
                            .map(|i| (a[i] as i32 - b[i] as i32).abs())
                            .max()
                            .unwrap();
                        if d > 2 {
                            *bad_rows.entry(y).or_insert(0usize) += 1;
                            *bad_cols.entry(x).or_insert(0usize) += 1;
                        }
                    }
                }
                let rows: Vec<String> = bad_rows
                    .iter()
                    .take(12)
                    .map(|(y, n)| format!("{y}:{n}"))
                    .collect();
                let cols: Vec<String> = bad_cols
                    .iter()
                    .take(12)
                    .map(|(x, n)| format!("{x}:{n}"))
                    .collect();
                eprintln!("[dbg] {} 错误行(前12): {}", kind.dir_name(), rows.join(" "));
                eprintln!("[dbg] {} 错误列(前12): {}", kind.dir_name(), cols.join(" "));
                // 调试：输出 R 通道网格（前 16×8）
                let mut grid = String::new();
                for y in 0..8 {
                    for x in 0..16 {
                        grid.push_str(&format!("{:3} ", mine.get_pixel(x, y).0[0]));
                    }
                    grid.push('\n');
                }
                eprintln!("[dbg] {} mine R 网格:\n{}", kind.dir_name(), grid);
            }
            assert!(
                max_lsb <= 2,
                "{} 与原版差异过大: max={max_lsb} LSB",
                kind.dir_name()
            );
        }
    }
}
