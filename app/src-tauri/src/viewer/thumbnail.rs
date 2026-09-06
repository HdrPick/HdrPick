//! 缩略图生成：解码 → 缩放至指定尺寸内（thumbnail 保比例）→ PNG → base64
//!
//! 内存 LRU 缓存：Mutex<HashMap> + 单调计数器时间戳，上限 600 项，
//! 超限时淘汰最久未使用项。前端拿到裸 base64 后自行拼 data URL。
//! 应用启动后可选后台线程异步预热（viewer.thumb_prewarm，默认关），
//! 未预热时首次浏览经 get_thumbnail 按需解码生成。

use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use image::{DynamicImage, ImageFormat};

use super::decode::{self, exr, jxl, jxr, pdf, png_hdr, psd};

/// 缩略图最长边上限（画册网格卡片高清显示：卡片宽约 180-360px，400px 覆盖 2x DPI）
const THUMB_SIZE: u32 = 400;
/// LRU 缓存上限（项）：600 张 × ~40KB(400px PNG) ≈ 24MB，可接受
const LRU_MAX: usize = 600;

/// 全局缓存：path → (base64 PNG, 最近使用时间戳)
static CACHE: OnceLock<Mutex<HashMap<String, (String, u64)>>> = OnceLock::new();
/// 单调递增计数器（LRU 时间戳）
static TICK: AtomicU64 = AtomicU64::new(0);

/// 获取缩略图（base64 PNG）：命中缓存直接返回，未命中解码生成并写入缓存
pub fn get_thumbnail(path: &str) -> Result<String, String> {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let tick = TICK.fetch_add(1, Ordering::Relaxed);

    // 1. 命中缓存：刷新 LRU 时间戳后返回
    {
        let mut m = cache.lock().map_err(|e| format!("缓存锁失败: {}", e))?;
        if let Some((b64, last)) = m.get_mut(path) {
            *last = tick;
            return Ok(b64.clone());
        }
    }

    // 2. 未命中：解码生成（锁外执行，避免长时间持锁阻塞并发请求）
    let b64 = generate(path)?;

    // 3. 写入缓存（超上限先淘汰最久未使用项）
    {
        let mut m = cache.lock().map_err(|e| format!("缓存锁失败: {}", e))?;
        while m.len() >= LRU_MAX {
            let oldest = m
                .iter()
                .min_by_key(|(_, (_, last))| *last)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    m.remove(&k);
                }
                None => break,
            }
        }
        m.insert(path.to_string(), (b64.clone(), tick));
    }
    Ok(b64)
}

/// 解码生成缩略图 base64（不带缓存）
fn generate(path: &str) -> Result<String, String> {
    let img = decode_for_thumbnail(Path::new(path))?;
    // thumbnail：保比例缩放至 THUMB_SIZE×THUMB_SIZE 框内（不放大）
    let thumb = img.thumbnail(THUMB_SIZE, THUMB_SIZE);

    // PNG 编码 → base64
    let mut buf = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
        .map_err(|e| format!("缩略图编码失败: {}", e))?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&buf))
}

/// 启动后台预热线程：应用启动后异步渐进填充内存缓存
///
/// 扫描全部相册源条目（时间新→旧），逐张生成缩略图写入 LRU：
/// - 低优先级慢慢跑（每张间隔 30ms），不与启动/UI 抢 CPU/IO
/// - 命中缓存/生成失败直接跳过（单张失败不中断整体）
/// - 已缓存的图打开时零解码延迟，整体响应显著提升
///
/// 配置开关 `viewer.thumb_prewarm`（默认 false）：机械盘/低配机上全库解码
/// 会与启动争抢 IO 洪峰；关闭时跳过预热，首次浏览经 get_thumbnail 按需生成
/// （路径不变：未命中 → 现场解码 → 写入 LRU，滚动浏览仍命中缓存）。
pub fn start_background_warmup() {
    if !crate::config::Config::load().viewer.thumb_prewarm {
        log::info!("缩略图预热已禁用（config viewer.thumb_prewarm=false），首次浏览将按需生成");
        return;
    }
    std::thread::Builder::new()
        .name("thumb-warmup".into())
        .spawn(|| {
            // 等主窗口完成首屏加载（避免与启动争抢磁盘）
            std::thread::sleep(std::time::Duration::from_secs(2));
            let st = match crate::viewer::library::library_state() {
                Ok(s) => s,
                Err(_) => return, // 相册未初始化：无预热目标
            };
            let mut paths: Vec<String> = st.entries.iter().map(|e| e.path.clone()).collect();
            // 时间新→旧（近期截图最常被浏览）
            let mtime_of = |p: &str| {
                st.entries
                    .iter()
                    .find(|e| e.path == p)
                    .map(|e| e.mtime)
                    .unwrap_or(0)
            };
            paths.sort_by(|a, b| mtime_of(b).cmp(&mtime_of(a)));

            let mut ok = 0usize;
            let mut total = 0usize;
            for p in paths {
                // 缩略图尺寸变更后旧缓存仍会被命中，预热只填空缺
                if get_thumbnail(&p).is_ok() {
                    ok += 1;
                }
                total += 1;
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            log::info!("缩略图后台预热完成：{}/{} 张已缓存", ok, total);
        })
        .map(|_| ())
        .unwrap_or_else(|e| {
            log::warn!("缩略图预热线程启动失败: {}", e);
        });
}

/// 按格式分类解码为内存图像（GIF 取首帧；AVIF image crate 无法解码，返回 Err 由前端占位）
fn decode_for_thumbnail(path: &Path) -> Result<DynamicImage, String> {
    match decode::classify_with_content(path) {
        // 原生格式 + TIFF + ICO：image crate 统一解码（按内容探测格式）
        decode::ImageKind::Native | decode::ImageKind::Tiff | decode::ImageKind::Ico => {
            image::ImageReader::open(path)
                .map_err(|e| format!("打开文件失败: {}", e))?
                .with_guessed_format()
                .map_err(|e| format!("识别格式失败: {}", e))?
                .decode()
                .map_err(|e| format!("解码失败: {}", e))
        }
        // PSD：psd crate 合成
        decode::ImageKind::Psd => psd::decode_image(path),
        // EXR：exr crate 解码 → 色调映射 SDR
        decode::ImageKind::Exr => exr::decode_image(path),
        // HDR PNG：PQ 解码 → 色调映射 SDR
        decode::ImageKind::PngHdr => png_hdr::decode_image(path),
        // JPEG XR：WIC 解码（HDR float 变体走色调映射 SDR）
        decode::ImageKind::Jxr => jxr::decode_image(path),
        // JPEG XL：libjxl 解码（HDR PQ / float 变体走色调映射 SDR）
        decode::ImageKind::Jxl => jxl::decode_image(path),
        // PDF：WinRT 首页渲染
        decode::ImageKind::Pdf => pdf::decode_image(path),
        // 不支持的格式
        _ => Err("暂不支持此格式".to_string()),
    }
}
