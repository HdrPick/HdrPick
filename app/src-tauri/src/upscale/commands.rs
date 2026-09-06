//! Tauri 命令层

use super::model::ModelKind;
use super::pipeline::{self, OutputFormat};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::Emitter;

/// 独立窗口批量取消标志（文件间检查）
static UPSCALE_CANCEL: AtomicBool = AtomicBool::new(false);

/// 批量进度事件（emit_to "upscale" 窗口）
#[derive(Clone, serde::Serialize)]
pub struct UpscaleProgress {
    pub index: usize,
    pub total: usize,
    pub name: String,
    /// start | done | error | cancelled | finish
    pub state: String,
    pub message: String,
}

fn parse_kind(model: &str) -> ModelKind {
    match model {
        "upconv7_photo" => ModelKind::Upconv7Photo,
        "upresnet10" => ModelKind::Upresnet10,
        "cunet" => ModelKind::Cunet,
        "realesrgan" => ModelKind::RealEsrganX4,
        "realesrgan_anime" => ModelKind::RealEsrganAnime6B,
        _ => ModelKind::Upconv7AnimeStyleArtRgb,
    }
}

/// 模型枚举（upscale_models 命令返回；前端下拉动态构建）
#[derive(Clone, serde::Serialize)]
pub struct UpscaleModelInfo {
    pub id: String,
    pub label: String,
    pub dir: String,
    pub has_scale: bool,
    /// 降噪+放大合并模型（noise{N}_scale2.0x 任一存在）
    pub has_noise_scale: bool,
    /// 纯降噪模型（noise{N}_model 任一存在）
    pub has_noise_only: bool,
    pub size_mb: f64,
}

/// 枚举可用模型家族（default_model_root 下实际文件探测）
#[tauri::command]
pub fn upscale_models() -> Vec<UpscaleModelInfo> {
    let root = pipeline::default_model_root();
    let entries: [(ModelKind, &str, &str); 6] = [
        (
            ModelKind::RealEsrganX4,
            "realesrgan",
            "照片 · Real-ESRGAN（推荐）",
        ),
        (
            ModelKind::RealEsrganAnime6B,
            "realesrgan_anime",
            "动漫 · Real-ESRGAN",
        ),
        (
            ModelKind::Upconv7AnimeStyleArtRgb,
            "upconv7_anime",
            "动漫图 · Upconv7",
        ),
        (ModelKind::Upconv7Photo, "upconv7_photo", "照片 · Upconv7"),
        (ModelKind::Upresnet10, "upresnet10", "通用 · UpResNet10"),
        (
            ModelKind::Cunet,
            "cunet",
            "动漫图 · CUnet（最高质量，较慢）",
        ),
    ];
    let mut out = Vec::new();
    for (kind, id, label) in entries {
        let dir = root.join(kind.dir_name());
        // Real-ESRGAN：.pth 可在 root 平铺或 root/RealESRGAN/ 子目录
        let has_pth = |f: &str| root.join(f).exists() || dir.join(f).exists();
        let has_file = |f: &str| dir.join(f).exists();
        // 模型体积（MB）：.pth 平铺按单文件；目录族按目录求和
        let size_mb = if kind.is_realesrgan() {
            has_pth_checked(&root, kind.scale_file())
        } else {
            std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
                        .sum::<u64>() as f64
                        / 1048576.0
                })
                .unwrap_or(0.0)
        };
        out.push(UpscaleModelInfo {
            id: id.to_string(),
            label: label.to_string(),
            dir: kind.dir_name().to_string(),
            has_scale: has_pth(kind.scale_file()),
            has_noise_scale: !kind.is_realesrgan()
                && (0..=3).any(|n| has_file(&kind.noise_scale_file(n))),
            has_noise_only: !kind.is_realesrgan() && (0..=3).any(|n| has_file(&kind.noise_file(n))),
            size_mb,
        });
    }
    out
}

/// pth 文件体积（平铺或子目录探测）
fn has_pth_checked(root: &std::path::Path, file: &str) -> f64 {
    for p in [root.join(file), root.join("RealESRGAN").join(file)] {
        if p.exists() {
            return std::fs::metadata(&p)
                .map(|m| m.len() as f64 / 1048576.0)
                .unwrap_or(0.0);
        }
    }
    0.0
}

/// GPU/CPU 基准（256×256 测试图 upconv7 2x 各一次）
///
/// async + spawn_blocking（照 upscale_window_run 模式）：GPU 基准 + CPU 全前向
/// 耗时数百 ms~秒级且持有 GPU engine 锁，同步命令跑在主线程会冻结 UI 并卡死动画播放
#[tauri::command]
pub async fn upscale_benchmark() -> Result<serde_json::Value, String> {
    tokio::task::spawn_blocking(|| {
        let root = pipeline::default_model_root();
        let model = super::model::WaifuModel::load(
            ModelKind::Upconv7AnimeStyleArtRgb,
            &root,
            ModelKind::Upconv7AnimeStyleArtRgb.scale_file(),
        )?;
        // 确定性测试图
        let mut img = image::RgbaImage::new(256, 256);
        for y in 0..256 {
            for x in 0..256 {
                img.put_pixel(
                    x,
                    y,
                    image::Rgba([
                        ((x * 7 + y * 3) % 256) as u8,
                        ((x * 5 + y * 11) % 256) as u8,
                        ((x * 13 + y * 2) % 256) as u8,
                        255,
                    ]),
                );
            }
        }
        let (rgb, _) = pipeline::rgba_to_tensors(&img);

        let t0 = std::time::Instant::now();
        let gpu = match pipeline::run_once_gpu(&model, &rgb) {
            Ok((_, ms)) => Some(ms),
            Err(e) => {
                log::info!("[upscale/bench] GPU 不可用: {}", e);
                None
            }
        };
        let gpu_wall = t0.elapsed().as_millis();

        let t1 = std::time::Instant::now();
        let _ = pipeline::run_once(&model, &rgb)?;
        let cpu_ms = t1.elapsed().as_millis();

        Ok(serde_json::json!({
            "gpu_ms": gpu,
            "gpu_wall_ms": gpu_wall,
            "cpu_ms": cpu_ms,
        }))
    })
    .await
    .map_err(|e| format!("任务失败: {}", e))?
}

/// 输出文件名后缀：_noise{N}_x{S}（S 去尾零）；宽高模式 _w{W}x{H}；纯降噪无倍率段
fn output_suffix(noise: Option<u32>, scale: Option<f32>, target: Option<(u32, u32)>) -> String {
    let mut s = String::new();
    if let Some(n) = noise {
        s.push_str(&format!("_noise{}", n));
    }
    if let Some((w, h)) = target {
        s.push_str(&format!("_w{}x{}", w, h));
    } else if let Some(sc) = scale {
        let v = format!("{}", sc);
        let v = v.trim_end_matches('0').trim_end_matches('.');
        s.push_str(&format!("_x{}", if v.is_empty() { "2" } else { v }));
    }
    s
}

/// 输出路径（同目录 + 防覆盖：存在则追加 _1/_2…）
fn unique_output(dir: &std::path::Path, stem: &str, suffix: &str, ext: &str) -> std::path::PathBuf {
    let base = dir.join(format!("{}_waifu2x{}.{}", stem, suffix, ext));
    if !base.exists() {
        return base;
    }
    for k in 1u32.. {
        let cand = dir.join(format!("{}_waifu2x{}_{}.{}", stem, suffix, k, ext));
        if !cand.exists() {
            return cand;
        }
    }
    base
}

/// 文件夹单层图片枚举（跳过本工具输出 <stem>_waifu2x*，避免原地重处理）
fn enumerate_folder_images(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    const EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let ext_ok = p
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| EXTS.iter().any(|e| x.eq_ignore_ascii_case(e)))
                .unwrap_or(false);
            if !ext_ok {
                continue;
            }
            let is_output = p
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.contains("_waifu2x"))
                .unwrap_or(false);
            if is_output {
                continue;
            }
            out.push(p);
        }
    }
    out.sort();
    out
}

/// 是否 JPEG（原版 auto_scale 语义：仅 JPEG 输入才降噪——有损压缩伪影）
fn is_jpeg(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
        .unwrap_or(false)
}

/// ===== 参数解析（两命令共用） =====

#[derive(Clone)]
struct UpscaleParams {
    kind: ModelKind,
    /// None = 纯降噪
    scale: Option<f32>,
    noise: Option<u32>,
    tta: bool,
    format: OutputFormat,
    /// SDR→HDR 增强档位（HDR 容器格式生效）
    hdr_enhance: crate::color::SdrToHdrPreset,
    /// 宽高模式（文件名后缀用）
    target: Option<(u32, u32)>,
}

/// 解析 mode/scale/noise；返回 Err 给出用户可读原因
///
/// mode: "scale" | "noise_scale" | "noise_only" | "auto"（auto = JPEG 才降噪）
#[allow(clippy::too_many_arguments)]
fn parse_params(
    path: &std::path::Path,
    model: Option<&str>,
    mode: Option<&str>,
    scale: Option<f64>,
    target_w: Option<u32>,
    target_h: Option<u32>,
    noise_level: Option<u32>,
    use_tta: Option<bool>,
    format: Option<&str>,
    hdr_enhance: Option<&str>,
    iw: u32,
    ih: u32,
) -> Result<UpscaleParams, String> {
    let kind = parse_kind(model.unwrap_or("upconv7_anime"));
    let tta = use_tta.unwrap_or(false);
    let format = format
        .and_then(OutputFormat::parse)
        .unwrap_or(OutputFormat::Png8);
    let hdr_enhance = hdr_enhance
        .and_then(crate::color::SdrToHdrPreset::parse)
        .unwrap_or_default();
    if hdr_enhance == crate::color::SdrToHdrPreset::Ai
        && !crate::color::SdrToHdrPreset::ai_model_ready()
    {
        return Err(
            "AI 智能档模型未就绪：请将 HDRTVNet 权重放到 models\\itm\\Ensemble_AGCM_LE.pth（当前回落自然档）"
                .into(),
        );
    }
    let noise = noise_level.map(|n| n.min(3));

    let mode = mode.unwrap_or("scale");
    // auto（原版 auto_scale）：仅 JPEG 输入才降噪
    let (want_noise, want_scale) = match mode {
        "noise_only" => (Some(noise.ok_or("纯降噪模式需选择降噪等级")?), false),
        "noise_scale" => (Some(noise.unwrap_or(1)), true),
        "auto" | "auto_scale" => (
            if is_jpeg(path) {
                Some(noise.unwrap_or(1))
            } else {
                None
            },
            true,
        ),
        // scale：忽略噪声参数，纯放大
        _ => (None, true),
    };

    // 目标倍率（宽高模式换算，原版 CalcScaleRatio：双指定取 max）
    let s = if want_scale {
        let v = pipeline::calc_scale_ratio(scale, target_w, target_h, iw, ih);
        if v <= 1.0 {
            if want_noise.is_some() {
                0.0 // ≤1 且带降噪 → 等效纯降噪
            } else {
                return Err("倍率需大于 1.0（或指定目标宽/高）".into());
            }
        } else {
            v
        }
    } else {
        0.0
    };
    let scale = if s > 1.0 { Some(s) } else { None };
    let target = if scale.is_none() && (target_w.is_some() || target_h.is_some()) {
        // 宽高模式但倍率 ≤1：仍按宽高模式记录
        Some((target_w.unwrap_or(iw), target_h.unwrap_or(ih)))
    } else if scale.is_some() && target_w.is_some() && target_h.is_some() {
        Some((target_w.unwrap(), target_h.unwrap()))
    } else {
        None
    };

    if scale.is_none() && want_noise.is_none() {
        return Err("纯降噪模式需选择降噪等级（或设置大于 1.0 的倍率）".into());
    }
    Ok(UpscaleParams {
        kind,
        scale,
        noise: want_noise,
        tta,
        format,
        hdr_enhance,
        target,
    })
}

/// 单图执行 + 保存 + sidecar 记录
fn process_one(
    root: &std::path::Path,
    params: &UpscaleParams,
    img: &image::RgbaImage,
    src_path: &std::path::Path,
    out_dir_override: Option<&std::path::Path>,
) -> Result<(std::path::PathBuf, u32, u32), String> {
    let t0 = std::time::Instant::now();
    let (out, _used_gpu) = match params.scale {
        Some(sc) => {
            pipeline::upscale_image_exact(root, params.kind, img, sc, params.noise, params.tta)?
        }
        None => pipeline::denoise_only(
            root,
            params.kind,
            img,
            params.noise.ok_or("纯降噪模式需选择降噪等级")?,
            params.tta,
        )?,
    };
    let (ow, oh) = (out.rgb.w as u32, out.rgb.h as u32);

    let dir =
        out_dir_override.unwrap_or_else(|| src_path.parent().unwrap_or(std::path::Path::new(".")));
    let stem = src_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let out_path = unique_output(
        dir,
        stem,
        &output_suffix(params.noise, params.scale, params.target),
        params.format.ext(),
    );
    out.save_hdr(&out_path, params.format, params.hdr_enhance)?;

    // sidecar 调参历史（原图同目录 .jietu-hdr/<stem>.upscale.json）
    record_history(
        src_path,
        &serde_json::json!({
            "time": chrono_now(),
            "model": params.kind.dir_name(),
            "scale": params.scale,
            "noise": params.noise,
            "tta": params.tta,
            "format": format!("{:?}", params.format),
            "hdr_enhance": format!("{:?}", params.hdr_enhance),
            "output": out_path.to_string_lossy(),
            "size": format!("{}x{}", ow, oh),
            "elapsed_ms": t0.elapsed().as_millis() as u64,
        }),
    );
    Ok((out_path, ow, oh))
}

fn chrono_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // 秒级时间戳（前端格式化显示）
    d.as_secs().to_string()
}

/// sidecar 调参历史（上限 50 条，原子写 tmp+rename）
fn sidecar_path(src: &std::path::Path) -> std::path::PathBuf {
    let dir = src.parent().unwrap_or(std::path::Path::new("."));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    dir.join(".jietu-hdr")
        .join(format!("{}.upscale.json", stem))
}

fn record_history(src: &std::path::Path, entry: &serde_json::Value) {
    let p = sidecar_path(src);
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(std::path::Path::new(".")));
    let mut doc: serde_json::Value = std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({ "entries": [] }));
    let entries = doc
        .as_object_mut()
        .map(|o| o.entry("entries").or_insert_with(|| serde_json::json!([])))
        .and_then(|v| v.as_array_mut());
    if let Some(arr) = entries {
        arr.push(entry.clone());
        while arr.len() > 50 {
            arr.remove(0);
        }
    }
    let tmp = p.with_extension("tmp");
    if let Ok(text) = serde_json::to_string_pretty(&doc) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// 读取某图的放大参数历史（面板回填上次参数用）
#[tauri::command]
pub fn upscale_history(path: String) -> serde_json::Value {
    let p = sidecar_path(std::path::Path::new(&path));
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({ "entries": [] }))
}

/// 独立 AI 放大窗口：批量执行（串行逐张，进度事件实时推送）
///
/// - files: 手选文件列表；folder: 文件夹（单层枚举图片）——二者可同时有效
/// - output_dir: 空 = 每张图原目录
/// - mode: scale / noise_scale / noise_only / auto（auto=JPEG 才降噪）
/// - scale: 任意倍率 >1；target_w/target_h: 目标宽高（CalcScaleRatio 语义）
#[tauri::command]
pub async fn upscale_window_run(
    app: tauri::AppHandle,
    files: Option<Vec<String>>,
    folder: Option<String>,
    output_dir: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    noise_level: Option<u32>,
    scale: Option<f64>,
    target_w: Option<u32>,
    target_h: Option<u32>,
    use_tta: Option<bool>,
    format: Option<String>,
    hdr_enhance: Option<String>,
) -> Result<String, String> {
    let mut inputs: Vec<std::path::PathBuf> = files
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();
    if let Some(dir) = folder.as_deref().filter(|d| !d.is_empty()) {
        let d = std::path::PathBuf::from(dir);
        if !d.is_dir() {
            return Err(format!("文件夹不存在: {}", dir));
        }
        inputs.extend(enumerate_folder_images(&d));
    }
    // 去重（手选 + 文件夹可能重叠）
    inputs.sort();
    inputs.dedup();
    if inputs.is_empty() {
        return Err("未选择输入（文件或文件夹）".into());
    }

    let out_dir = output_dir
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from);
    if let Some(d) = &out_dir {
        if !d.is_dir() {
            return Err(format!("输出目录不存在: {}", d.display()));
        }
    }

    UPSCALE_CANCEL.store(false, Ordering::Relaxed);
    let total = inputs.len();
    let app_emit = app.clone();
    let summary = tokio::task::spawn_blocking(move || {
        let root = pipeline::default_model_root();
        let mut ok = 0usize;
        let mut fail = 0usize;
        for (i, p) in inputs.into_iter().enumerate() {
            if UPSCALE_CANCEL.load(Ordering::Relaxed) {
                let _ = app_emit.emit_to(
                    "upscale",
                    "upscale://progress",
                    UpscaleProgress {
                        index: i,
                        total,
                        name: String::new(),
                        state: "cancelled".into(),
                        message: format!("已取消（完成 {}/{}）", ok + fail, total),
                    },
                );
                return format!("已取消：成功 {} / 失败 {}", ok, fail);
            }

            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string();
            let _ = app_emit.emit_to(
                "upscale",
                "upscale://progress",
                UpscaleProgress {
                    index: i,
                    total,
                    name: name.clone(),
                    state: "start".into(),
                    message: "处理中…".into(),
                },
            );

            let t0 = std::time::Instant::now();
            let res = image::open(&p)
                .map_err(|e| format!("打开失败: {}", e))
                .and_then(|img| {
                    let rgba = img.to_rgba8();
                    let (iw, ih) = (rgba.width(), rgba.height());
                    let params = parse_params(
                        &p,
                        model.as_deref(),
                        mode.as_deref(),
                        scale,
                        target_w,
                        target_h,
                        noise_level,
                        use_tta,
                        format.as_deref(),
                        hdr_enhance.as_deref(),
                        iw,
                        ih,
                    )?;
                    process_one(&root, &params, &rgba, &p, out_dir.as_deref())
                        .map(|(path, ow, oh)| (path, iw, ih, ow, oh))
                });
            match res {
                Ok((out_path, iw, ih, ow, oh)) => {
                    let msg = format!(
                        "{}×{} → {}×{}，{:.1}s → {}",
                        iw,
                        ih,
                        ow,
                        oh,
                        t0.elapsed().as_secs_f32(),
                        out_path.display()
                    );
                    log::info!("[upscale] {} {}", name, msg);
                    ok += 1;
                    let _ = app_emit.emit_to(
                        "upscale",
                        "upscale://progress",
                        UpscaleProgress {
                            index: i + 1,
                            total,
                            name: name.clone(),
                            state: "done".into(),
                            message: msg,
                        },
                    );
                }
                Err(e) => {
                    fail += 1;
                    log::error!("[upscale] {} {}", name, e);
                    let _ = app_emit.emit_to(
                        "upscale",
                        "upscale://progress",
                        UpscaleProgress {
                            index: i + 1,
                            total,
                            name: name.clone(),
                            state: "error".into(),
                            message: e,
                        },
                    );
                }
            }
        }
        format!("完成：成功 {} / 失败 {}（共 {}）", ok, fail, total)
    })
    .await
    .map_err(|e| format!("任务失败: {}", e))?;

    let _ = app.emit_to(
        "upscale",
        "upscale://progress",
        UpscaleProgress {
            index: total,
            total,
            name: String::new(),
            state: "finish".into(),
            message: summary.clone(),
        },
    );
    Ok(summary)
}

/// 取消批量（当前张完成后停止）
#[tauri::command]
pub fn upscale_window_cancel() {
    UPSCALE_CANCEL.store(true, Ordering::Relaxed);
}

/// AI 放大（看图浮动面板）：GPU 优先，任意倍率/目标宽高 + 纯降噪 + auto + TTA + 多格式
///
/// - model: upconv7_anime（默认）/ upconv7_photo / upresnet10 / cunet
/// - mode: scale / noise_scale / noise_only / auto（auto=JPEG 输入才降噪）
/// - scale: 任意倍率 >1；target_w/target_h: 目标宽高；均无 → 纯降噪
/// - noise_level: 0-3（0=最轻降噪）
/// - format: png8（默认）/ png16 / jxl（HDR PQ16）
///
/// 输出 <stem>_waifu2x[_noiseN][_xS|_wWxH].<ext> 在原图同目录；返回结果消息（前端 toast）
#[tauri::command]
pub async fn upscale_p1_test(
    path: String,
    model: Option<String>,
    mode: Option<String>,
    scale: Option<f64>,
    target_w: Option<u32>,
    target_h: Option<u32>,
    noise_level: Option<u32>,
    use_tta: Option<bool>,
    format: Option<String>,
) -> Result<String, String> {
    let p = std::path::PathBuf::from(&path);
    let img = image::open(&p)
        .map_err(|e| format!("打开失败: {}", e))?
        .to_rgba8();
    let (iw, ih) = (img.width(), img.height());

    let params = parse_params(
        &p,
        model.as_deref(),
        mode.as_deref(),
        scale,
        target_w,
        target_h,
        noise_level,
        use_tta,
        format.as_deref(),
        None,
        iw,
        ih,
    )?;

    let root = pipeline::default_model_root();
    let t0 = std::time::Instant::now();
    let pr = params.clone();
    let (out_path, ow, oh, used_gpu, tta) = tokio::task::spawn_blocking(move || {
        let (out, used_gpu) = match pr.scale {
            Some(sc) => pipeline::upscale_image_exact(&root, pr.kind, &img, sc, pr.noise, pr.tta)?,
            None => pipeline::denoise_only(
                &root,
                pr.kind,
                &img,
                pr.noise.ok_or("纯降噪模式需选择降噪等级")?,
                pr.tta,
            )?,
        };
        let dir = p.parent().unwrap_or(std::path::Path::new("."));
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let out_path = unique_output(
            dir,
            stem,
            &output_suffix(pr.noise, pr.scale, pr.target),
            pr.format.ext(),
        );
        out.save(&out_path, pr.format)?;
        record_history(
            &p,
            &serde_json::json!({
                "time": chrono_now(),
                "model": pr.kind.dir_name(),
                "scale": pr.scale,
                "noise": pr.noise,
                "tta": pr.tta,
                "format": format!("{:?}", pr.format),
                "output": out_path.to_string_lossy(),
                "size": format!("{}x{}", out.rgb.w, out.rgb.h),
                "elapsed_ms": t0.elapsed().as_millis() as u64,
            }),
        );
        Ok::<_, String>((
            out_path,
            out.rgb.w as u32,
            out.rgb.h as u32,
            used_gpu,
            pr.tta,
        ))
    })
    .await
    .map_err(|e| format!("任务失败: {}", e))??;

    let msg = format!(
        "完成({}{}{})：{}×{} → {}×{}，耗时 {:.1}s → {}",
        if used_gpu { "GPU" } else { "CPU" },
        if tta { "+TTA" } else { "" },
        match params.noise {
            Some(n) => format!("+降噪{}", n),
            None => String::new(),
        },
        iw,
        ih,
        ow,
        oh,
        t0.elapsed().as_secs_f32(),
        out_path.display()
    );
    log::info!("[upscale] {}", msg);
    Ok(msg)
}

/// 截图联动：对已保存截图后台 AI 2x 增强（生成增强副本，不替换原图）
///
/// 设置开关开启后由保存链路调用（spawn 后台，不阻塞截图流程）
pub fn enhance_capture_background(path: &std::path::Path) {
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        let root = pipeline::default_model_root();
        let img = match image::open(&p) {
            Ok(i) => i.to_rgba8(),
            Err(e) => {
                log::warn!("[upscale/capture] 打开截图失败: {}", e);
                return;
            }
        };
        let params = UpscaleParams {
            kind: ModelKind::Upconv7AnimeStyleArtRgb,
            scale: Some(2.0),
            noise: None,
            tta: false,
            format: OutputFormat::Png8,
            hdr_enhance: crate::color::SdrToHdrPreset::Off,
            target: None,
        };
        match process_one(&root, &params, &img, &p, None) {
            Ok((out, _, _)) => log::info!("[upscale/capture] 截图增强完成: {}", out.display()),
            Err(e) => log::warn!("[upscale/capture] 截图增强失败: {}", e),
        }
    });
}
