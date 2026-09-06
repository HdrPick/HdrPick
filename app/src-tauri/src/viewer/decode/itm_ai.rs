//! 看图 SDR→HDR（AI 档）：复用 `upscale::itm` 的 HDRTVNet（AI 放大「AI 智能档」同一网络）
//!
//! 本模块只做看图管线 glue：解码 SDR → 模型推理（权重 OnceLock 缓存，原
//! `enhance_image_nits` 每次调用都重载磁盘）→ strength 混合 → `HdrSource` 入缓存。
//! 真 HDR 显示（open_hdr_viewer）/ 亮度调节（TonePanel）/ 调参管线零改动复用。
//!
//! 与 `upscale/itm.rs` 的关系：模型与算子在那里；区别于未来可能的 GPU 化 ExecLayer。
//!
//! 混合语义（「AI 反转强度」滑杆）：nits 域 lerp——
//! `nits_out = lerp(sdr_nits, ai_nits, strength)`，
//! sdr_nits = sRGB 线性 × 实测 SDR 白电平（strength=0 → 原 SDR 观感；1 → 纯模型输出）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
// ImageDecoder trait 须在作用域内才能调用 orientation()（image 0.25.10）
use image::ImageDecoder;

use super::{cache_hdr_source, take_hdr_source, HdrSource};
use crate::upscale::itm::{default_model_path, enhance_image_nits_with, HdrItm};

// ==================== 模型与状态缓存 ====================

/// 模型权重缓存：加载成功后常驻（2.3MB 解析一次）；
/// 加载失败不缓存（用户补放模型文件后下次点击即生效，无需重启）
static ITM_MODEL: OnceLock<Mutex<Result<HdrItm, String>>> = OnceLock::new();

/// 持模型引用执行 f（未加载/上次失败时先重新加载）
fn with_model<T>(f: impl FnOnce(&HdrItm) -> Result<T, String>) -> Result<T, String> {
    let cell = ITM_MODEL.get_or_init(|| Mutex::new(Err("未加载".to_string())));
    let mut guard = cell.lock().map_err(|e| format!("模型锁失败: {e}"))?;
    if guard.is_err() {
        let loaded = HdrItm::load(&default_model_path()).map_err(|e| {
            format!("AI 模型加载失败（{}）: {e}", default_model_path().display())
        });
        log::info!(
            "[ITM] 模型加载: {}",
            loaded.as_ref().map(|_| "成功").unwrap_or("失败")
        );
        *guard = loaded;
    }
    f(guard.as_ref().map_err(|e| e.clone())?)
}

/// 已转换参数侧表：itm key → strength（显示窗口缓存未命中时按此重跑）
static ITM_PARAMS: OnceLock<Mutex<HashMap<String, f32>>> = OnceLock::new();

fn params_map() -> &'static Mutex<HashMap<String, f32>> {
    ITM_PARAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 解码缓存（RGBA8，LRU-4）：滑杆重跑免解码
static ITM_INPUT: OnceLock<Mutex<HashMap<String, (SdrDecode, u64)>>> = OnceLock::new();
static ITM_INPUT_TICK: AtomicU64 = AtomicU64::new(0);
const ITM_INPUT_MAX: usize = 4;

struct SdrDecode {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

// ==================== 键与工具 ====================

/// ITM 合成键：`itm:<源文件路径>`（真实 Windows 路径不会以 itm: 开头，无歧义）
pub fn itm_key(path: &Path) -> String {
    format!("itm:{}", path.to_string_lossy())
}

/// 从合成键还原真实源路径
pub fn parse_itm_key(key: &Path) -> Option<PathBuf> {
    key.to_string_lossy()
        .strip_prefix("itm:")
        .map(PathBuf::from)
}

/// SDR 图像门：看图管线可解码的格式（HDR 线性源不在此列——它们原生就是 HDR）
fn sdr_gate(path: &Path) -> bool {
    matches!(
        super::classify_with_content(path),
        super::ImageKind::Native
            | super::ImageKind::Tiff
            | super::ImageKind::Ico
            | super::ImageKind::Psd
            | super::ImageKind::Pdf
    )
}

// ==================== 解码与打包 ====================

/// 解码 SDR 图 → RGBA8（EXIF orientation 修正），入缓存（LRU-4）
fn sdr_rgba_of(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let key = path.to_string_lossy().into_owned();
    if let Some(m) = ITM_INPUT.get() {
        if let Ok(mut m) = m.lock() {
            let tick = ITM_INPUT_TICK.fetch_add(1, Ordering::Relaxed);
            if let Some((d, t)) = m.get_mut(&key) {
                *t = tick;
                return Ok((d.w, d.h, d.rgba.clone()));
            }
        }
    }

    // 解码
    let mut img = image::ImageReader::open(path)
        .map_err(|e| format!("打开图片失败: {e}"))?
        .with_guessed_format()
        .map_err(|e| format!("识别图片格式失败: {e}"))?
        .decode()
        .map_err(|e| format!("解码图片失败: {e}"))?;
    // EXIF 方向修正（image 0.25：ImageDecoder::orientation 需 &mut decoder；
    // 浏览器 <img> 自动旋转，HDR 视图需对齐观感）
    if let Ok(mut decoder) = image::ImageReader::open(path)
        .map_err(|e| format!("打开图片失败: {e}"))?
        .with_guessed_format()
        .map_err(|e| format!("识别图片格式失败: {e}"))?
        .into_decoder()
    {
        let orientation = decoder
            .orientation()
            .unwrap_or(image::metadata::Orientation::NoTransforms);
        if orientation != image::metadata::Orientation::NoTransforms {
            img.apply_orientation(orientation);
        }
    }
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let data = rgba.into_raw();

    // 写缓存（LRU 淘汰）
    if let Some(m) = ITM_INPUT.get() {
        if let Ok(mut m) = m.lock() {
            let tick = ITM_INPUT_TICK.fetch_add(1, Ordering::Relaxed);
            while m.len() >= ITM_INPUT_MAX {
                if let Some(oldest) = m
                    .iter()
                    .min_by_key(|(_, (_, t))| *t)
                    .map(|(k, _)| k.clone())
                {
                    m.remove(&oldest);
                } else {
                    break;
                }
            }
            m.insert(
                key.clone(),
                (
                    SdrDecode {
                        w,
                        h,
                        rgba: data.clone(),
                    },
                    tick,
                ),
            );
        }
    }
    Ok((w, h, data))
}

/// nits（BT.709 线性 CHW）→ HdrSource（scRGB f16 RGBA 交错，1.0=80nits）
/// 打包口径复刻 upscale/pipeline.rs::scrgb_texture_from_nits
fn nits_to_hdr_source(w: u32, h: u32, nits: &[f32]) -> HdrSource {
    let np = (w * h) as usize;
    let f16 = crate::viewer::decode::exr::f32_to_f16;
    let mut data = vec![0u8; np * 8];
    for i in 0..np {
        let o = i * 8;
        let lin = |c: usize| f16(nits[c * np + i] / 80.0);
        data[o..o + 2].copy_from_slice(&lin(0).to_le_bytes());
        data[o + 2..o + 4].copy_from_slice(&lin(1).to_le_bytes());
        data[o + 4..o + 6].copy_from_slice(&lin(2).to_le_bytes());
        data[o + 6..o + 8].copy_from_slice(&f16(1.0).to_le_bytes()); // A = 0x3C00
    }
    HdrSource {
        width: w,
        height: h,
        data,
    }
}

// ==================== 核心转换 ====================

/// SDR→HDR（AI）：解码 → HDRTVNet 推理 → strength 混合 → HdrSource
///
/// - strength ∈ [0,1]：0 = 原 SDR 观感（映射到实测 SDR 白电平），1 = 纯模型输出
/// - 不写缓存（命令层负责）；CPU 推理 1080p 数秒，调用方放阻塞线程
pub fn run_itm_ai(path: &Path, strength: f32) -> Result<HdrSource, String> {
    let t0 = std::time::Instant::now();
    if !sdr_gate(path) {
        return Err("该格式已是 HDR 源或不受支持，无需 SDR→HDR".to_string());
    }
    let (w, h, rgba) = sdr_rgba_of(path)?;
    if w < 32 || h < 32 {
        return Err(format!("图片过小（{w}×{h}），AI 模型要求至少 32×32"));
    }
    if (w as u64) * (h as u64) * 8 > 512 * 1024 * 1024 {
        return Err("图片过大，暂不支持 SDR→HDR".to_string());
    }

    // RGBA8 → sRGB[0,1] RGB CHW（模型输入契约，不做线性化）
    let np = (w * h) as usize;
    let mut sdr = vec![0f32; np * 3];
    for i in 0..np {
        let p = &rgba[i * 4..i * 4 + 3];
        sdr[i] = p[0] as f32 / 255.0;
        sdr[np + i] = p[1] as f32 / 255.0;
        sdr[2 * np + i] = p[2] as f32 / 255.0;
    }

    // HDRTVNet 推理（权重缓存命中时秒级启动）
    let mut ai_nits = with_model(|m| {
        crate::upscale::itm::enhance_image_nits_with(m, &sdr, w as usize, h as usize)
    })?;

    // strength 混合（nits 域）：sdr_nits = sRGB 线性 × 实测 SDR 白电平
    let sdr_white = crate::capture::monitor::get_sdr_white_level_nits();
    let s = strength.clamp(0.0, 1.0);
    if s < 1.0 {
        for i in 0..np {
            for c in 0..3 {
                let lin_c =
                    crate::color::srgb_eotf(rgba[i * 4 + c] as f32 / 255.0) * sdr_white;
                ai_nits[c * np + i] = lin_c + (ai_nits[c * np + i] - lin_c) * s;
            }
        }
    }

    let src = nits_to_hdr_source(w, h, &ai_nits);
    log::info!(
        "[ITM] SDR→HDR 完成: {} ({w}×{h}, strength={s:.2}, {:.1}s)",
        path.display(),
        t0.elapsed().as_secs_f32()
    );
    Ok(src)
}

/// 显示窗口缓存恢复：命中直接返回；未命中按侧表参数重跑 ITM（日志留痕）
pub fn ensure_itm_source(itm_key_path: &Path) -> Option<HdrSource> {
    let key = itm_key_path.to_string_lossy().into_owned();
    if let Some(src) = take_hdr_source(itm_key_path) {
        return Some(src);
    }
    let strength = params_map()
        .lock()
        .ok()
        .and_then(|m| m.get(&key).copied())
        .unwrap_or(0.7);
    log::info!("[ITM] 缓存未命中，重跑: {key} (strength={strength:.2})");
    let real = parse_itm_key(itm_key_path)?;
    match run_itm_ai(&real, strength) {
        Ok(src) => {
            cache_hdr_source(itm_key_path, src.clone());
            Some(src)
        }
        Err(e) => {
            log::error!("[ITM] 重跑失败: {e}");
            None
        }
    }
}

// ==================== 命令 ====================

/// ITM 转换结果（前端 interface ItmInfo）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItmInfo {
    /// 合成键（itm:<源路径>），open_hdr_viewer / retonemap 用
    pub itm_path: String,
    pub width: u32,
    pub height: u32,
    /// 推理耗时（毫秒，含首次模型加载）
    pub elapsed_ms: u64,
}

/// SDR→HDR（AI 档）：解码 → HDRTVNet → HdrSource 入缓存（key = itm:<path>）
///
/// 重活（解码 + CPU 推理数秒）由 async fn 自动进线程池。
#[tauri::command]
pub async fn itm_to_hdr(path: String, strength: Option<f32>) -> Result<ItmInfo, String> {
    let p = PathBuf::from(&path);
    let strength = strength.unwrap_or(0.7).clamp(0.0, 1.0);
    let started = std::time::Instant::now();
    let run_path = p.clone();
    let result = tokio::task::spawn_blocking(move || run_itm_ai(&run_path, strength))
        .await
        .map_err(|e| format!("ITM 任务异常: {e}"))
        .and_then(|r| r);
    match result {
        Ok(src) => {
            let key = itm_key(&p);
            let (w, h) = (src.width, src.height);
            cache_hdr_source(Path::new(&key), src);
            if let Ok(mut m) = params_map().lock() {
                m.insert(key.clone(), strength);
            }
            Ok(ItmInfo {
                itm_path: key,
                width: w,
                height: h,
                elapsed_ms: started.elapsed().as_millis() as u64,
            })
        }
        Err(e) => {
            log::warn!("[ITM] SDR→HDR 失败 {}: {e}", path);
            Err(e)
        }
    }
}
