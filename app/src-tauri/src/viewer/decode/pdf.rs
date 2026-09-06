//! PDF 解码（WinRT Windows.Data.Pdf，系统组件）：按页渲染位图供看图模式显示
//!
//! 多页支持（2345 看图王同类行为）：open_image 返回 page_count，
//! 前端翻页调用 pdf_render_page 渲染指定页；
//! 打印路径（print.rs pages_from_pdf）走独立多页渲染管线，互不影响。
//!
//! 临时文件命名带页码后缀（pdf_<hash>_p<N>.png）：同文件不同页不互相覆盖，
//! 且翻页 URL 变化保证 <img> 重新加载（与亮度调节版本化路径同坑同解）。
//!
//! 线程模型：调用方（Tauri 线程池 / 缩略图预热线程）独立 COM STA 初始化，
//! 与 print.rs 同一套路（S_FALSE = 该线程已初始化，无需重复 Uninitialize）。

use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::StorageFile;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

use super::temp_png_path;

/// 首页预览渲染 DPI（A4 纵向约 1654×2339 像素：屏幕显示与缩放余量的平衡值）
const PREVIEW_DPI: f32 = 200.0;

/// 渲染 PDF 指定页 → DynamicImage（独立 COM STA）
fn render_page(path: &Path, page_index: u32, dpi: f32) -> Result<image::DynamicImage, String> {
    let co_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let co_ok = co_hr.is_ok();
    let result = render_page_inner(path, page_index, dpi);
    if co_ok {
        unsafe { CoUninitialize() };
    }
    result
}

fn render_page_inner(
    path: &Path,
    page_index: u32,
    dpi: f32,
) -> Result<image::DynamicImage, String> {
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))
        .map_err(|e| format!("打开 PDF 失败: {}", e))?
        .get()
        .map_err(|e| format!("打开 PDF 失败: {}", e))?;
    let doc = PdfDocument::LoadFromFileAsync(&file)
        .map_err(|e| format!("加载 PDF 失败: {}", e))?
        .get()
        .map_err(|e| format!("加载 PDF 失败: {}", e))?;
    let count = doc
        .PageCount()
        .map_err(|e| format!("读取 PDF 页数失败: {}", e))?;
    if count == 0 {
        return Err("PDF 无页面".to_string());
    }
    if page_index >= count {
        return Err(format!(
            "页码越界: 第 {} 页 / 共 {} 页",
            page_index + 1,
            count
        ));
    }

    let page = doc
        .GetPage(page_index)
        .map_err(|e| format!("读取 PDF 第 {} 页失败: {}", page_index + 1, e))?;
    // Size 单位 DIP（1/96 英寸）→ 渲染 DPI 像素
    let size = page
        .Size()
        .map_err(|e| format!("读取页面尺寸失败: {}", e))?;
    let w_px = (size.Width.max(1.0) * dpi / 96.0).ceil() as u32;
    let h_px = (size.Height.max(1.0) * dpi / 96.0).ceil() as u32;

    let opts = PdfPageRenderOptions::new().map_err(|e| format!("初始化渲染参数失败: {}", e))?;
    let _ = opts.SetDestinationWidth(w_px);
    let _ = opts.SetDestinationHeight(h_px);

    let stream = InMemoryRandomAccessStream::new().map_err(|e| format!("创建渲染流失败: {}", e))?;
    page.RenderWithOptionsToStreamAsync(&stream, &opts)
        .map_err(|e| format!("渲染 PDF 页面失败: {}", e))?
        .get()
        .map_err(|e| format!("渲染 PDF 页面失败: {}", e))?;

    let size = stream.Size().unwrap_or(0u64) as u32;
    if size == 0 {
        return Err("PDF 渲染结果为空".to_string());
    }
    let input = stream
        .GetInputStreamAt(0)
        .map_err(|e| format!("读取渲染结果失败: {}", e))?;
    let reader =
        DataReader::CreateDataReader(&input).map_err(|e| format!("读取渲染结果失败: {}", e))?;
    reader
        .LoadAsync(size)
        .map_err(|e| format!("读取渲染结果失败: {}", e))?
        .get()
        .map_err(|e| format!("读取渲染结果失败: {}", e))?;
    let mut png = vec![0u8; size as usize];
    reader
        .ReadBytes(&mut png)
        .map_err(|e| format!("读取渲染结果失败: {}", e))?;

    image::load_from_memory(&png).map_err(|e| format!("解码渲染结果失败: {}", e))
}

/// 看图模式：渲染 PDF 首页（decode_image 分发用）
pub fn decode_image(path: &Path) -> Result<image::DynamicImage, String> {
    render_page(path, 0, PREVIEW_DPI)
}

/// 渲染首页 → 临时 PNG（decode_to_temp_png 分发用）
pub fn decode_to_temp_png(path: &Path) -> Result<(PathBuf, u32, u32), String> {
    let img = decode_image(path)?;
    let (w, h) = (img.width(), img.height());
    let temp = super::tiff::write_temp_png(&img, path)?;
    Ok((temp, w, h))
}

/// 读取 PDF 页数（独立 COM STA；打开文档为轻操作）
pub fn page_count(path: &Path) -> Result<u32, String> {
    let co_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let co_ok = co_hr.is_ok();
    let result = (|| {
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))
            .map_err(|e| format!("打开 PDF 失败: {}", e))?
            .get()
            .map_err(|e| format!("打开 PDF 失败: {}", e))?;
        let doc = PdfDocument::LoadFromFileAsync(&file)
            .map_err(|e| format!("加载 PDF 失败: {}", e))?
            .get()
            .map_err(|e| format!("加载 PDF 失败: {}", e))?;
        doc.PageCount()
            .map_err(|e| format!("读取 PDF 页数失败: {}", e))
    })();
    if co_ok {
        unsafe { CoUninitialize() };
    }
    result
}

/// 渲染指定页 → 带页码后缀的临时 PNG（翻页用；同文件不同页不覆盖、URL 变化触发重载）
pub fn render_page_to_temp_png(
    path: &Path,
    page_index: u32,
) -> Result<(PathBuf, u32, u32), String> {
    let img = render_page(path, page_index, PREVIEW_DPI)?;
    let (w, h) = (img.width(), img.height());
    let temp = pdf_temp_path(path, page_index);
    img.save(&temp)
        .map_err(|e| format!("写入临时 PNG 失败: {:#}", e))?;
    Ok((temp, w, h))
}

/// 带页码的临时路径：%TEMP%\jietu-hdr\pdf_<hash>_p<N>.png
fn pdf_temp_path(src: &Path, page_index: u32) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    src.to_string_lossy().hash(&mut hasher);
    let dir = std::env::temp_dir().join("jietu-hdr");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("pdf_{:016x}_p{}.png", hasher.finish(), page_index))
}
