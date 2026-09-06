//! 看图后端模块（viewer 窗口，设计文档 2.6）
//!
//! 7 个命令：open_image / list_directory_images / decode_image / get_thumbnail /
//! get_exif / convert_image / copy_image_to_clipboard。
//!
//! 分层策略（decode/）：
//! - Tier 1 浏览器原生（png/jpg/jpeg/gif/bmp/webp/avif）：url 返回原始路径字符串，
//!   前端自行 convertFileSrc；Rust 仅读字节头判格式与宽高
//! - Tier 2/3（tiff/ico/psd）：解码 RGBA → PNG 写 %TEMP%\jietu-hdr\decode_<hash>.png，
//!   url 返回临时文件路径
//!
//! 重解码命令声明为 async fn：Tauri 自动将其放入线程池执行，不阻塞主线程。

pub mod albums;
pub mod anim_pool;
pub mod batch;
pub mod convert;
pub mod decode;
pub mod exif;
pub mod hdr_viewer;
pub mod library;
pub mod print;
pub mod recycle;
pub mod thumbnail;
pub mod tonemap_history;
pub mod vault;

use std::path::{Path, PathBuf};

use serde::Serialize;

// ==================== 对外数据结构（字段名与前端 TypeScript interface 严格对齐） ====================

/// 打开的图片信息（前端 ViewerWindow.vue interface ImageInfo）
#[derive(Debug, Clone, Serialize)]
pub struct ImageInfo {
    /// 图片地址：原生格式为原始路径字符串（前端自行 convertFileSrc）；
    /// tiff/ico/psd 为临时 PNG 路径
    pub url: String,
    /// 宽（像素）；无法解析时为 0（前端 img onload 用 naturalWidth 兜底）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 规范化格式名（png/jpeg/gif/bmp/webp/avif/tiff/ico/psd）
    pub format: String,
    /// 帧数（GIF 动画统计 Image Descriptor；其余为 1）
    pub frame_count: u32,
    /// 页数（仅 PDF 为实际页数；其余为 1）
    pub page_count: u32,
    /// 是否含 EXIF（仅 JPEG/TIFF 可能为 true）
    pub has_exif: bool,
}

/// 同目录图片列表项（前端 interface ImageEntry）
#[derive(Debug, Clone, Serialize)]
pub struct ImageEntry {
    /// 完整路径
    pub path: String,
    /// 文件名（含扩展名）
    pub name: String,
    /// 宽（像素；解析失败为 0）
    pub width: u32,
    /// 高（像素；解析失败为 0）
    pub height: u32,
}

/// EXIF 分组数据（前端 interface ExifData）
#[derive(Debug, Clone, Serialize)]
pub struct ExifData {
    /// 分组列表（无 EXIF 时为空数组）
    pub groups: Vec<ExifGroup>,
}

/// EXIF 分组
#[derive(Debug, Clone, Serialize)]
pub struct ExifGroup {
    /// 分组标题（如"设备与拍摄"/"曝光"/"GPS"）
    pub title: String,
    /// 组内字段
    pub items: Vec<ExifItem>,
}

/// EXIF 单项
#[derive(Debug, Clone, Serialize)]
pub struct ExifItem {
    /// 字段标签（中文，如"制造商"）
    pub label: String,
    /// 字段值（字符串）
    pub value: String,
}

// ==================== 命令 ====================

/// 打开图片：按扩展名分发——原生格式返回原始路径 + 字节头尺寸；
/// tiff/ico/psd 等解码为临时 PNG；PDF 渲染首页为临时 PNG
#[tauri::command]
pub async fn open_image(path: String) -> Result<ImageInfo, String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("文件不存在: {}", path));
    }
    build_image_info(&p).map_err(|e| {
        log::warn!("打开图片失败 {}: {}", path, e);
        e
    })
}

/// 列出同目录图片（按文件名不区分大小写排序，与资源管理器习惯一致）
#[tauri::command]
pub async fn list_directory_images(dir: String) -> Result<Vec<ImageEntry>, String> {
    let d = PathBuf::from(&dir);
    if !d.is_dir() {
        return Err(format!("目录不存在: {}", dir));
    }

    let mut list = Vec::new();
    let entries = std::fs::read_dir(&d).map_err(|e| format!("读取目录失败: {}", e))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // 扩展名过滤（与前端打开对话框过滤器一致）
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if !matches!(
            ext.as_str(),
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "bmp"
                | "webp"
                | "avif"
                | "tif"
                | "tiff"
                | "ico"
                | "psd"
                | "jxl"
                | "exr"
                | "jxr"
                | "wdp"
                | "pdf"
        ) {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // 快速尺寸（读头不完整解码；单个文件失败为 0 不阻断列表）
        let (w, h) = decode::quick_dimensions(&path);
        list.push(ImageEntry {
            path: path.to_string_lossy().into_owned(),
            name,
            width: w,
            height: h,
        });
    }
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(list)
}

/// 非原生格式统一解码入口（前端对原生格式也可调用，行为等同 open_image）
#[tauri::command]
pub async fn decode_image(path: String) -> Result<ImageInfo, String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("文件不存在: {}", path));
    }
    build_image_info(&p)
}

/// PDF 翻页：渲染指定页（1 基页码）→ 临时 PNG 的 ImageInfo
/// 重活（WinRT 渲染 + PNG 编码）由 async fn 自动进线程池，不阻塞主线程
#[tauri::command]
pub async fn pdf_render_page(path: String, page: u32) -> Result<ImageInfo, String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("文件不存在: {}", path));
    }
    let count = decode::pdf::page_count(&p)?;
    if page < 1 || page > count {
        return Err(format!("页码越界: 第 {} 页 / 共 {} 页", page, count));
    }
    let (temp, w, h) = decode::pdf::render_page_to_temp_png(&p, page - 1)?;
    log::info!("PDF 翻页: {} 第 {}/{} 页 ({}x{})", path, page, count, w, h);
    Ok(ImageInfo {
        url: temp.to_string_lossy().into_owned(),
        width: w,
        height: h,
        format: "pdf".to_string(),
        frame_count: 1,
        page_count: count,
        has_exif: false,
    })
}

/// 「亮度调节」即时重渲：用指定色调映射参数把缓存的 HDR 源重渲为 SDR
///
/// HDR 源（scRGB 纹理）在首次打开时缓存（decode::cache_hdr_source），
/// 此命令仅重跑 tone map（毫秒级），滑杆拖动实时预览。
/// preset: "builtin:soft" 等预设 id；overrides: 前端滑杆覆盖值（None = 跟随预设）
#[tauri::command]
pub async fn retonemap_image(
    path: String,
    preset: Option<String>,
    overrides: Option<crate::config::ToneMapOverrides>,
) -> Result<ImageInfo, String> {
    let p = PathBuf::from(&path);
    let params = {
        let mut config = crate::config::Config::load();
        if let Some(id) = preset.as_deref() {
            config.tonemap_settings.active_preset = id.to_string();
        }
        if let Some(ov) = overrides {
            config.tonemap_settings.overrides = ov;
        }
        config.active_tonemap_params(crate::capture::monitor::get_sdr_white_level_nits())
    };
    let Some(result) = decode::retonemap_to_temp_png(&p, &params) else {
        return Err("HDR 源不在缓存（请重新打开图片）".to_string());
    };
    let (temp, w, h) = result?;
    log::info!("亮度调节重渲: {} ({}x{})", p.display(), w, h);
    Ok(ImageInfo {
        url: temp.to_string_lossy().into_owned(),
        width: w,
        height: h,
        format: String::new(),
        frame_count: 1,
        page_count: 1,
        has_exif: false,
    })
}

/// 获取缩略图：解码 → 200×200 内保比例缩放 → PNG → base64 字符串（LRU 缓存 100 项）
#[tauri::command]
pub async fn get_thumbnail(path: String) -> Result<String, String> {
    thumbnail::get_thumbnail(&path)
}

/// 获取 EXIF 分组数据（无 EXIF 返回空 groups）
#[tauri::command]
pub async fn get_exif(path: String) -> Result<ExifData, String> {
    exif::parse(Path::new(&path))
}

/// 转格式另存为：src 解码 → 按 format（png/jpeg/webp）编码写 dst；quality 1-100（仅 jpeg 生效）
#[tauri::command]
pub async fn convert_image(
    src: String,
    dst: String,
    format: String,
    quality: u32,
) -> Result<(), String> {
    let result = convert::convert(Path::new(&src), Path::new(&dst), &format, quality);
    match &result {
        Ok(()) => log::info!("转格式完成: {} → {} ({})", src, dst, format),
        Err(e) => log::warn!("转格式失败 {} → {}: {}", src, dst, e),
    }
    result
}

/// 复制图片到剪贴板
///
/// 动图（gif/webp）走 CFHDROP 文件列表通道——保留整个文件（多帧动画完整，
/// 粘贴到微信/QQ/Word 等均得到动画而非首帧）；静态图仍走 tauri 剪贴板位图
/// 通道（可直接粘贴到画图/Photoshop 像素编辑）。
#[tauri::command]
pub fn copy_image_to_clipboard(app: tauri::AppHandle, path: String) -> Result<(), String> {
    copy_image_to_clipboard_impl(&app, &path)
}

/// 复制实现（viewer 命令与主面板 copy_image_file_to_clipboard 共用）
///
/// - GIF：HTML Format（`<img src="file:///...">`）。QQ/微信等富文本输入框
///   按 HTML 粘贴并读取 src 指向的文件 → 图片消息（GIF 动画保留）。
///   不写位图（QQ 读到即首帧化）、不写 CF_HDROP（QQ 按"发送文件"处理）、
///   不用 data URI（多 MB base64 会导致 QQ 粘贴卡顿，实测）
/// - 其余格式：位图贴图（webp 动图取首帧，Windows 位图无动画语义）
pub fn copy_image_to_clipboard_impl(app: &tauri::AppHandle, path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(format!("文件不存在: {}", path));
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    if ext == "gif" {
        return copy_gif_to_clipboard(path);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
    let dynimg = image::load_from_memory(&bytes).map_err(|e| format!("解码图像失败: {}", e))?;
    let rgba = dynimg.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let img = tauri::image::Image::new_owned(rgba.into_raw(), w, h);
    write_clipboard_image_with_retry(app, &img)
}

/// GIF 复制：HTML Format 单格式（file:/// 本地路径引用）
///
/// "HTML Format"（CF_HTML 规范，微软 HTML Clipboard Format）fragment 为
/// `<img src="file:///本地绝对路径">`——QQ 富文本粘贴读 src 文件 → 动图。
/// 剪贴板数据仅数百字节（无卡顿风险）；路径需 URL 编码（空格/中文）。
fn copy_gif_to_clipboard(path: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    // --- CF_HTML 数据（file:/// 引用本地文件） ---
    let file_url = format!("file:///{}", path.replace('\\', "/"));
    let fragment = format!("<img src=\"{}\">", file_url);
    let cf_html = build_cf_html(&fragment);

    // --- HGLOBAL 分配（内容拷贝） ---
    let alloc_copy = |data: &[u8]| -> Result<windows::Win32::Foundation::HGLOBAL, String> {
        let hg = unsafe { GlobalAlloc(GMEM_MOVEABLE, data.len()) }
            .map_err(|e| format!("GlobalAlloc 失败: {}", e))?;
        unsafe {
            let base = GlobalLock(hg);
            if base.is_null() {
                return Err("GlobalLock 失败".to_string());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), base as *mut u8, data.len());
            let _ = GlobalUnlock(hg);
        }
        Ok(hg)
    };
    let hg_html = alloc_copy(&cf_html)?;

    // --- "HTML Format" 命名格式 ID（CF_HTML 官方注册名） ---
    let html_name: Vec<u16> = "HTML Format\0".encode_utf16().collect();
    let cf_html_id = unsafe { RegisterClipboardFormatW(PCWSTR(html_name.as_ptr())) };
    if cf_html_id == 0 {
        let _ = global_free(hg_html);
        return Err("注册剪贴板格式 \"HTML Format\" 失败".to_string());
    }

    // --- OpenClipboard（瞬态占用重试） → 写入 ---
    let mut opened = false;
    for attempt in 1..=8 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            if attempt > 1 {
                log::info!("OpenClipboard 第 {} 次重试成功", attempt);
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !opened {
        let _ = global_free(hg_html);
        return Err("打开剪贴板失败：剪贴板被其他程序持续占用".to_string());
    }

    let result = (|| {
        if let Err(e) = unsafe { EmptyClipboard() } {
            return Err(format!("清空剪贴板失败: {}", e));
        }
        // 成功即所有权移交系统（失败侧仍归我们，须释放）
        match unsafe { SetClipboardData(cf_html_id, HANDLE(hg_html.0)) } {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = global_free(hg_html);
                Err(format!("SetClipboardData 失败: {}", e))
            }
        }
    })();
    let _ = unsafe { CloseClipboard() };
    if result.is_ok() {
        log::info!(
            "GIF 已按 HTML Format 写入剪贴板（src={}，{}B）",
            file_url,
            cf_html.len()
        );
    }
    result
}

/// 构造 CF_HTML 数据（微软 HTML Clipboard Format 规范）
///
/// 头部（Version/StartHTML/EndHTML/StartFragment/EndFragment，偏移零填充
/// 10 位）+ `<html><body>` 上下文 + `<!--StartFragment-->` 标记 + 片段。
/// 偏移与标记双提供（解析器两种定位方式都支持）；全 ASCII（base64 与
/// 标签），UTF-8 字节序即 ASCII。
fn build_cf_html(fragment: &str) -> Vec<u8> {
    let pre = "<html><body>";
    let marker_start = "<!--StartFragment-->";
    let marker_end = "<!--EndFragment-->";
    let post = "</body></html>";
    // 先按固定宽度（每偏移 10 位）计算头部长度，再回填偏移
    let header_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let start_html = header_len;
    // StartFragment/EndFragment 指向片段正文（标记内侧），与 Chrome 实现一致
    let start_fragment = start_html + pre.len() + marker_start.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = start_html
        + pre.len()
        + marker_start.len()
        + fragment.len()
        + marker_end.len()
        + post.len();
    let mut out = String::with_capacity(end_html);
    out.push_str(&format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        start_html, end_html, start_fragment, end_fragment
    ));
    out.push_str(pre);
    out.push_str(marker_start);
    out.push_str(fragment);
    out.push_str(marker_end);
    out.push_str(post);
    out.into_bytes()
}

/// GlobalFree（失败侧句柄清理；成功侧所有权已移交系统不可再 free）
fn global_free(h: windows::Win32::Foundation::HGLOBAL) -> windows::core::Result<()> {
    unsafe { windows::Win32::Foundation::GlobalFree(h).map(|_| ()) }
}

/// 剪贴板写入重试：OpenClipboard 被其他程序瞬态占用是常态
/// （剪贴板管理器/输入法监听轮询），Windows 标准做法是短间隔重试；
/// 持续占用则如实报错（不再静默吞掉伪装成功）
pub fn write_clipboard_image_with_retry(
    app: &tauri::AppHandle,
    img: &tauri::image::Image,
) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    const ATTEMPTS: u32 = 8;
    const DELAY_MS: u64 = 20;
    let mut last_err = String::new();
    for attempt in 1..=ATTEMPTS {
        match app.clipboard().write_image(img) {
            Ok(()) => {
                if attempt > 1 {
                    log::info!("剪贴板写入第 {} 次重试成功", attempt);
                }
                return Ok(());
            }
            Err(e) => {
                last_err = e.to_string();
                if attempt < ATTEMPTS {
                    log::warn!(
                        "剪贴板写入失败（第 {}/{} 次，{}ms 后重试）: {}",
                        attempt,
                        ATTEMPTS,
                        DELAY_MS,
                        e
                    );
                    std::thread::sleep(std::time::Duration::from_millis(DELAY_MS));
                }
            }
        }
    }
    Err(format!(
        "写入剪贴板失败: {}（剪贴板被其他程序持续占用，请检查剪贴板管理类软件）",
        last_err
    ))
}

// ==================== 设计稿融合：壁纸 / 软件回收站（设计稿 3.2/3.3） ====================

/// 设为桌面壁纸（设计稿 3.2 壁纸一键设置）
///
/// SPI_SETDESKWALLPAPER 仅支持 bmp/jpg/png；webp/gif 等先转 PNG 临时文件再设置。
#[tauri::command]
pub fn set_wallpaper(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if !p.is_file() {
        return Err(format!("文件不存在: {}", path));
    }
    // 非壁纸兼容格式 → 转临时 PNG
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    let target: PathBuf = if matches!(ext.as_str(), "bmp" | "jpg" | "jpeg" | "png") {
        p.to_path_buf()
    } else {
        let tmp = std::env::temp_dir()
            .join("jietu-hdr")
            .join(format!("wallpaper_{}.png", std::process::id()));
        convert::convert(p, &tmp, "png", 100)?;
        tmp
    };

    // SystemParametersInfoW（SPI_SETDESKWALLPAPER=20，SPIF_UPDATEINIFILE|SPIF_SENDCHANGE=3）
    extern "system" {
        fn SystemParametersInfoW(
            action: u32,
            uiparam: u32,
            pvparam: *mut std::ffi::c_void,
            winini: u32,
        ) -> i32;
    }
    let mut wide: Vec<u16> = target
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe { SystemParametersInfoW(20, 0, wide.as_mut_ptr().cast(), 3) };
    if ok == 0 {
        return Err("设置壁纸失败（SystemParametersInfoW 返回 0）".into());
    }
    log::info!("已设置桌面壁纸: {}", path);
    Ok(())
}

/// 移入软件回收站（设计稿 3.3：删除先进回收站，7 天可恢复）
#[tauri::command]
pub fn recycle_image(path: String) -> Result<recycle::RecycleEntry, String> {
    recycle::recycle_file(&path)
}

/// 列出回收站（自动过滤 7 天过期项）
#[tauri::command]
pub fn list_recycle() -> Result<Vec<recycle::RecycleEntry>, String> {
    recycle::list_files()
}

/// 从回收站恢复到原始路径，返回恢复后的完整路径
#[tauri::command]
pub fn restore_recycle(recycledName: String) -> Result<String, String> {
    recycle::restore_file(&recycledName)
}

/// 彻底删除单条回收项
#[tauri::command]
pub fn purge_recycle(recycledName: String) -> Result<(), String> {
    recycle::purge_file(&recycledName)
}

/// 清空回收站，返回清理数量
#[tauri::command]
pub fn purge_all_recycle() -> Result<usize, String> {
    recycle::purge_all()
}

/// 保存任意图像字节到指定路径（滤镜编辑 / 拼图导出的前端 Canvas 产物落盘）
#[tauri::command]
pub fn save_image_blob(path: String, data: Vec<u8>) -> Result<(), String> {
    // 目标父目录不存在时自动创建（如输出目录子路径）
    if let Some(parent) = Path::new(&path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
    }
    std::fs::write(&path, &data).map_err(|e| format!("写入文件失败: {}", e))?;
    log::info!("已保存图像 {} 字节 -> {}", data.len(), path);
    Ok(())
}

// ==================== 内部辅助 ====================

/// 构造 ImageInfo：按分类分发（原生 probe 头 / 其余解码临时 PNG）
fn build_image_info(p: &Path) -> Result<ImageInfo, String> {
    // 规范化扩展名（ImageInfo.format 用）
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    let kind = decode::classify_with_content(p);
    match kind {
        // Tier 1 浏览器原生：url = 原始路径，宽高读字节头
        decode::ImageKind::Native => {
            // 字节头探测（AVIF 返回零尺寸由前端兜底；头损坏时也降级为零尺寸而非报错）
            let (format, w, h, frames) = match decode::native::probe(p) {
                Ok(info) => (info.format, info.width, info.height, info.frame_count),
                Err(_) => (ext.clone(), 0, 0, 1),
            };
            Ok(ImageInfo {
                url: p.to_string_lossy().into_owned(),
                width: w,
                height: h,
                format,
                frame_count: frames,
                page_count: 1,
                has_exif: exif::has_exif(p),
            })
        }
        // Tier 2/3：解码 → 临时 PNG，url = 临时文件路径（PDF 渲染首页）
        decode::ImageKind::Tiff
        | decode::ImageKind::Ico
        | decode::ImageKind::Psd
        | decode::ImageKind::Exr
        | decode::ImageKind::PngHdr
        | decode::ImageKind::Jxr
        | decode::ImageKind::Jxl
        | decode::ImageKind::Pdf => {
            let is_pdf = matches!(kind, decode::ImageKind::Pdf);
            // PDF 统一走带页码后缀的临时路径（与翻页 pdf_render_page 命名一致）
            let (temp, w, h) = if is_pdf {
                decode::pdf::render_page_to_temp_png(p, 0)?
            } else {
                decode::decode_to_temp_png(p)?
            };
            let format = match kind {
                decode::ImageKind::PngHdr => "png-hdr".to_string(),
                decode::ImageKind::Jxr => "jxr".to_string(),
                decode::ImageKind::Jxl => "jxl".to_string(),
                _ if ext == "tif" => "tiff".to_string(),
                _ => ext,
            };
            // PDF 页数（读取失败降级为 1 = 单页浏览不阻塞打开）
            let pages = if is_pdf {
                decode::pdf::page_count(p).unwrap_or(1)
            } else {
                1
            };
            Ok(ImageInfo {
                url: temp.to_string_lossy().into_owned(),
                width: w,
                height: h,
                format,
                frame_count: 1,
                page_count: pages,
                has_exif: exif::has_exif(p),
            })
        }
        // 阶段外格式
        decode::ImageKind::Unsupported => Err("暂不支持此格式".to_string()),
    }
}
