//! 解码分层统一入口：按扩展名分发（设计文档 2.6 多格式解码分层策略）
//!
//! - Tier 1 浏览器原生（png/jpg/jpeg/gif/bmp/webp/avif）→ native.rs 读字节头判格式与宽高，
//!   url 用原始路径，前端 <img> 直载（GIF 动画保留）
//! - Tier 2 image crate（tif/tiff/ico）→ 解码 RGBA → 临时 PNG
//! - Tier 3 专用库（psd → psd crate；exr / HDR PNG / JXL / JXR → 色调映射为 SDR 预览；
//!   pdf → WinRT 首页渲染）

pub mod exr;
/// 混合解码管线骨架（路线 B；P4+ 条件立项，当前无功能）
pub mod hybrid;
pub mod hybrid_gpu;
/// 看图 SDR→HDR（AI 档）：复用 upscale::itm 的 HDRTVNet，产出 HdrSource 入缓存
pub mod itm_ai;
pub mod jxl;
pub mod jxr;
pub mod native;
pub mod pdf;
/// native 播放出口 GPU 化（PQ16 → scRGB f16 纹理直写，jxl.rs PQ16 直出配套）
pub mod pq16_gpu;
pub mod png_hdr;
pub mod psd;
pub mod tiff;

use std::path::{Path, PathBuf};

/// HDR 源图（归一化为 scRGB f16 线性光纹理，1.0 = 80 nits 绝对语义）
///
/// 由 exr / png_hdr / jxl / jxr 解码时提取，供「亮度调节」即时重渲：
/// tone map 是纯函数，换参数重跑毫秒级，无需重新解码源文件。
#[derive(Clone)]
pub struct HdrSource {
    pub width: u32,
    pub height: u32,
    /// 4 × f16 = 8 bytes/px（RGBA，A=1.0），BT.709 原色
    pub data: Vec<u8>,
}

impl HdrSource {
    /// 构造 CapturedTexture（to_sdr 管线入参）
    pub fn to_texture(&self) -> crate::capture::CapturedTexture {
        crate::capture::CapturedTexture {
            width: self.width,
            height: self.height,
            format: crate::color::PixelFormat::R16g16b16a16Float,
            data: self.data.clone(),
            row_pitch: self.width as usize * 8,
            via_gdi: false,
        }
    }
}

/// HDR 源缓存（LRU 8 张：当前图 + 相邻图 + 文件夹预解码集）
///
/// key = 源文件路径；「亮度调节」滑杆拖动时反复命中，仅重跑 tone map。
/// 8 张 2560×1600 ≈ 260MB（scRGB f16 8B/px），文件夹内切换瞬时完成。
static HDR_SOURCE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, (HdrSource, u64)>>,
> = std::sync::OnceLock::new();
static HDR_CACHE_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 10 = 8 张原生 HDR 源 + ITM 转换结果与浏览余量（ITM 键 itm:<path> 共用本缓存）
const HDR_CACHE_MAX: usize = 10;

fn hdr_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, (HdrSource, u64)>> {
    HDR_SOURCE_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 缓存 HDR 源（解码时调用；超上限按 LRU 淘汰）
pub fn cache_hdr_source(path: &Path, src: HdrSource) {
    if let Ok(mut m) = hdr_cache().lock() {
        let tick = HDR_CACHE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        while m.len() >= HDR_CACHE_MAX {
            let oldest = m
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    m.remove(&k);
                }
                None => break,
            }
        }
        m.insert(path.to_string_lossy().into_owned(), (src, tick));
    }
}

/// 取缓存 HDR 源（重渲命令用；命中刷新 LRU）
pub fn take_hdr_source(path: &Path) -> Option<HdrSource> {
    let tick = HDR_CACHE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut m) = hdr_cache().lock() {
        if let Some((src, t)) = m.get_mut(&path.to_string_lossy().into_owned()) {
            *t = tick;
            return Some(src.clone());
        }
    }
    None
}

/// 播放链路剖析计时开关（prof）：env `JXL_PROF=1` 时 println 分段耗时，
/// 默认关（热路径零开销——仅首次 env 读取 + 布尔快照）。
pub(crate) fn prof_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("JXL_PROF").map(|v| v == "1").unwrap_or(false))
}

/// 剖析 GPU 阶段同步开关：env `JXL_PROF_SYNC=1` 时每个 pass 后 flush，
/// 用于把 GPU 各 pass 耗时从"读回大锅"里拆出来（剖析模式专用，会降吞吐）。
pub(crate) fn prof_sync() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("JXL_PROF_SYNC").map(|v| v == "1").unwrap_or(false))
}

/// 行级并行像素转换（std::thread::scope，零新依赖）
///
/// out 按行切成互斥可变片，分发给工作线程（≤8）；每线程持有若干行的
/// 独立 &mut（编译期保证不相交），输入侧共享只读。
/// f(out_row, row_index)：转换一行（row_bytes = 一行的字节数）。
pub fn par_rows_mut<F>(out: &mut [u8], row_bytes: usize, f: F)
where
    F: Fn(&mut [u8], usize) + Send + Sync,
{
    if row_bytes == 0 || out.is_empty() {
        return;
    }
    let rows = out.len() / row_bytes;
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8);
    // 小图 / 单核：直接单线程（线程开销 > 收益）
    if nthreads <= 1 || rows < 64 {
        for (y, row) in out.chunks_mut(row_bytes).enumerate() {
            f(row, y);
        }
        return;
    }
    let chunk_rows = (rows + nthreads - 1) / nthreads;
    // chunks_mut 产生的各 &mut [u8] 互不相交 → 可分别 move 进 scoped 线程
    let mut row_chunks: Vec<&mut [u8]> = out.chunks_mut(row_bytes).collect();
    std::thread::scope(|s| {
        for (t, group) in row_chunks.chunks_mut(chunk_rows).enumerate() {
            let base = t * chunk_rows;
            let f = &f;
            s.spawn(move || {
                for (i, row) in group.iter_mut().enumerate() {
                    f(row, base + i);
                }
            });
        }
    });
}

/// 确保 HDR 源可用：优先 LRU 缓存；未命中则完整解码一次（解码内部会缓存）
///
/// 真 HDR 查看窗口（viewer/hdr_viewer.rs）的数据入口——scRGB f16 纹理
/// 直传 D3D11 scRGB 交换链，不做任何色调映射（1.0 = 80 nits 绝对语义）。
///
/// 竞态防护（"偶尔解码失败"根因）：解码器在色调映射/写临时 PNG **之前**
/// 插入缓存（间隔 200-400ms），期间并发 prewarm_folder 可能插入 8 张同
/// 目录图把本图挤出 LRU-8 → take 落空。解码后未命中自动重试（最多 2 轮，
/// 第二轮插入后立即取，窗口极小），全部落空才返回 None。
pub fn ensure_hdr_source(path: &Path) -> Option<HdrSource> {
    if let Some(src) = take_hdr_source(path) {
        return Some(src);
    }
    match classify_with_content(path) {
        ImageKind::Exr | ImageKind::PngHdr | ImageKind::Jxl | ImageKind::Jxr => {
            for attempt in 1..=2 {
                match decode_to_temp_png(path) {
                    Err(e) => {
                        log::warn!(
                            "[真HDR] 源解码失败(第{}次) {}: {}",
                            attempt,
                            path.display(),
                            e
                        );
                    }
                    Ok(_) => {}
                }
                if let Some(src) = take_hdr_source(path) {
                    if attempt > 1 {
                        log::info!(
                            "[真HDR] 源就绪（重试{}次后命中，此前被 LRU 挤出）: {}",
                            attempt,
                            path.display()
                        );
                    }
                    return Some(src);
                }
                log::warn!(
                    "[真HDR] 解码后缓存未命中（LRU 被并发预解码挤出？第{}次）: {}",
                    attempt,
                    path.display()
                );
            }
            None
        }
        _ => None,
    }
}

/// 用指定参数把 HDR 源重渲为 SDR 临时 PNG（「亮度调节」即时预览核心）
///
/// 返回 (临时 PNG 路径, 宽, 高)；源不在缓存时返回 None（前端回退重新打开）。
/// 每次重渲写入带序号的独立临时文件（decode_<hash>_r<seq>.png）：
/// URL 唯一变化才能触发前端 <img> 重新加载（同路径覆盖写会被 WebView 缓存）。
pub fn retonemap_to_temp_png(
    path: &Path,
    params: &crate::capture::HdrToSdrParams,
) -> Option<Result<(PathBuf, u32, u32), String>> {
    static RETONEMAP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let src = take_hdr_source(path)?;
    let sdr = crate::capture::hdr_pipeline::to_sdr(&src.to_texture(), params);
    let img = image::RgbaImage::from_raw(src.width, src.height, sdr.rgba)?;
    // 版本化路径（同路径覆盖写不触发 <img> 刷新，必须唯一）
    let seq = RETONEMAP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = temp_png_path(path);
    let dir = base.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("decode")
        .to_string();
    let temp = dir.join(format!("{}_r{}.png", stem, seq));
    let result = (|| -> Result<PathBuf, String> {
        let file = std::fs::File::create(&temp).map_err(|e| format!("创建临时文件失败: {}", e))?;
        let mut w = std::io::BufWriter::new(file);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut w, image::ImageFormat::Png)
            .map_err(|e| format!("PNG 编码失败: {}", e))?;
        Ok(temp)
    })();
    Some(result.map(|p| (p, src.width, src.height)))
}

/// 图片分类（按扩展名）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// 浏览器原生：png / jpg / jpeg / gif / bmp / webp / avif
    Native,
    /// TIFF（image crate tiff feature）
    Tiff,
    /// ICO（image crate ico feature）
    Ico,
    /// PSD（psd crate）
    Psd,
    /// EXR（exr crate，HDR 截图：色调映射为 SDR 预览）
    Exr,
    /// HDR PNG（16bit PQ + cICP：PQ 解码 + 色调映射为 SDR 预览）
    PngHdr,
    /// JPEG XR（WIC 解码；HDR float 变体走色调映射 SDR 预览）
    Jxr,
    /// JPEG XL（vendor jxl-sys 解码；HDR PQ / float 变体走色调映射 SDR 预览）
    Jxl,
    /// PDF（WinRT Windows.Data.Pdf 首页渲染）
    Pdf,
    /// 不支持的格式
    Unsupported,
}

/// 按扩展名分类（小写比较，无扩展名视为不支持）
pub fn classify(path: &Path) -> ImageKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "avif" => ImageKind::Native,
        "tif" | "tiff" => ImageKind::Tiff,
        "ico" => ImageKind::Ico,
        "psd" => ImageKind::Psd,
        "exr" => ImageKind::Exr,
        "jxr" | "wdp" => ImageKind::Jxr,
        "jxl" => ImageKind::Jxl,
        "pdf" => ImageKind::Pdf,
        _ => ImageKind::Unsupported,
    }
}

/// 扩展名 + 内容探测分类：PNG 需读头部判 cICP(PQ) HDR PNG
///
/// HDR PNG（16bit PQ + cICP）走 `<img>` 直显会被 WebView2 当 sRGB
/// （画面极暗），必须转色调映射 SDR 预览路径。
pub fn classify_with_content(path: &Path) -> ImageKind {
    match classify(path) {
        ImageKind::Native => {
            let is_png = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("png"))
                .unwrap_or(false);
            if is_png && png_hdr::is_hdr_png(path) {
                ImageKind::PngHdr
            } else {
                ImageKind::Native
            }
        }
        other => other,
    }
}

/// 源路径对应的临时解码 PNG 路径：%TEMP%\jietu-hdr\decode_<hash>.png
///
/// 同一路径稳定映射到同一临时文件（重复打开复用，覆盖写即可）。
pub fn temp_png_path(src: &Path) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    src.to_string_lossy().hash(&mut hasher);
    let dir = std::env::temp_dir().join("jietu-hdr");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("decode_{:016x}.png", hasher.finish()))
}

/// 非原生格式统一解码：按分类分发到 tiff / ico（image crate）、psd 或 exr crate
///
/// 返回 (临时 PNG 路径, 宽, 高)
pub fn decode_to_temp_png(path: &Path) -> Result<(PathBuf, u32, u32), String> {
    match classify_with_content(path) {
        ImageKind::Tiff => tiff::decode_to_temp_png(path),
        ImageKind::Ico => ico_decode_to_temp_png(path),
        ImageKind::Psd => psd::decode_to_temp_png(path),
        ImageKind::Exr => exr::decode_to_temp_png(path),
        ImageKind::PngHdr => png_hdr::decode_to_temp_png(path),
        ImageKind::Jxr => jxr::decode_to_temp_png(path),
        ImageKind::Jxl => jxl::decode_to_temp_png(path),
        ImageKind::Pdf => pdf::decode_to_temp_png(path),
        ImageKind::Native => Err("浏览器原生格式无需解码".to_string()),
        ImageKind::Unsupported => Err("暂不支持此格式".to_string()),
    }
}

/// ICO 解码：image crate ico feature（多尺寸取最大帧）→ 临时 PNG
fn ico_decode_to_temp_png(path: &Path) -> Result<(PathBuf, u32, u32), String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| format!("打开文件失败: {}", e))?
        .with_guessed_format()
        .map_err(|e| format!("识别格式失败: {}", e))?
        .decode()
        .map_err(|e| format!("ICO 解码失败: {}", e))?;
    let (w, h) = (img.width(), img.height());
    let temp = tiff::write_temp_png(&img, path)?;
    Ok((temp, w, h))
}

/// 解码为内存图像（全格式统一入口）：
/// - HDR 变体（EXR / HDR PNG / JXL / JXR）→ **当前激活预设**色调映射为 SDR
///   （设置菜单切换预设后重新调用即得新效果——重新输出/转格式的数据源）
/// - SDR / 原生格式 → 原样解码
pub fn decode_image(path: &Path) -> Result<image::DynamicImage, String> {
    match classify_with_content(path) {
        ImageKind::Native | ImageKind::Tiff | ImageKind::Ico => image::ImageReader::open(path)
            .map_err(|e| format!("打开文件失败: {}", e))?
            .with_guessed_format()
            .map_err(|e| format!("识别格式失败: {}", e))?
            .decode()
            .map_err(|e| format!("解码失败: {}", e)),
        ImageKind::Psd => psd::decode_image(path),
        ImageKind::Exr => exr::decode_image(path),
        ImageKind::PngHdr => png_hdr::decode_image(path),
        ImageKind::Jxr => jxr::decode_image(path),
        ImageKind::Jxl => jxl::decode_image(path),
        ImageKind::Pdf => pdf::decode_image(path),
        ImageKind::Unsupported => Err("暂不支持此格式".to_string()),
    }
}

/// 快速读取图片尺寸（目录列表扫描用，尽量只读头不完整解码）
///
/// 失败时返回 (0, 0) 占位（单个文件异常不阻断整目录列表）。
pub fn quick_dimensions(path: &Path) -> (u32, u32) {
    match classify(path) {
        // 原生格式：字节头解析（AVIF 等解析不出时为 0）
        ImageKind::Native => native::probe(path)
            .map(|i| (i.width, i.height))
            .unwrap_or((0, 0)),
        // TIFF：image crate content 探测 + 头部尺寸
        ImageKind::Tiff => image::ImageReader::open(path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.into_dimensions().ok())
            .unwrap_or((0, 0)),
        // PSD：8BPS 头解析（避免整文件读取）
        ImageKind::Psd => read_head(path, 64)
            .and_then(|h| native::probe_psd_dimensions(&h))
            .unwrap_or((0, 0)),
        // ICO：目录项声明尺寸（0 表示 256）
        ImageKind::Ico => read_head(path, 16)
            .and_then(|h| native::probe_ico_dimensions(&h))
            .unwrap_or((0, 0)),
        // JXL：libjxl BasicInfo（解码至事件即停）
        ImageKind::Jxl => jxl::quick_dimensions(path),
        // EXR/JXR/PDF/未知：占位
        _ => (0, 0),
    }
}

/// 读取文件头部 n 字节（quick_dimensions 辅助）
fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    let mut read = 0;
    while read < n {
        match f.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(k) => read += k,
            Err(_) => return None,
        }
    }
    buf.truncate(read);
    Some(buf)
}
