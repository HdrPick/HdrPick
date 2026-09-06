//! Windows OCR 文字识别（WinRT Windows.Media.Ocr）
//!
//! 使用系统内置 OCR 引擎识别截图中的文字，返回每行文字及其在图像中的
//! 像素坐标矩形，供前端在截图上叠加"可选择复制"的文字层。
//!
//! 引擎选择策略：优先中文（zh 引擎可同时识别中英文混排），
//! 其次英文，否则第一个可用语言。
//! 运行在独立 STA 线程（COINIT_APARTMENTTHREADED），避免 WinRT
//! 对象与 Tauri 主线程的套间冲突。

use serde::Serialize;

/// OCR 识别出的单个词（含图像像素坐标矩形）
#[derive(Clone, Debug, Serialize)]
pub struct OcrWordInfo {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// OCR 识别出的一行文字（含图像像素坐标矩形）
#[derive(Clone, Debug, Serialize)]
pub struct OcrLineInfo {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub words: Vec<OcrWordInfo>,
}

/// 对 PNG 图片执行 OCR，返回识别的文字行（坐标为图像像素）
/// `language`: None/空 = 自动选择；否则使用指定 BCP-47 标签（如 zh-Hans / en-US）
pub fn run_ocr(png_path: &str, language: Option<&str>) -> Result<Vec<OcrLineInfo>, String> {
    let path = png_path.to_string();
    let lang = language.map(|s| s.to_string());
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        let result = ocr_impl(&path, lang.as_deref());
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
        result
    })
    .join()
    .map_err(|_| "OCR 工作线程崩溃".to_string())?
}

/// 列出系统已安装的 OCR 识别语言标签（BCP-47）
pub fn available_languages() -> Result<Vec<String>, String> {
    use windows::Media::Ocr::OcrEngine;

    unsafe {
        let available = OcrEngine::AvailableRecognizerLanguages()
            .map_err(|e| format!("查询 OCR 语言失败: {e}"))?;
        let mut tags: Vec<String> = Vec::new();
        for l in &available {
            if let Ok(t) = l.LanguageTag() {
                tags.push(t.to_string());
            }
        }
        Ok(tags)
    }
}

fn ocr_impl(png_path: &str, language: Option<&str>) -> Result<Vec<OcrLineInfo>, String> {
    use windows::Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, SoftwareBitmap,
    };
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::FileAccessMode;
    use windows::Storage::StorageFile;

    unsafe {
        // 1. 读取 PNG 文件并解码为 SoftwareBitmap
        let file = StorageFile::GetFileFromPathAsync(&windows::core::HSTRING::from(png_path))
            .map_err(|e| format!("打开图片失败: {e}"))?
            .get()
            .map_err(|e| format!("读取图片失败: {e}"))?;
        let stream = file
            .OpenAsync(FileAccessMode::Read)
            .map_err(|e| format!("打开图片流失败: {e}"))?
            .get()
            .map_err(|e| format!("等待图片流失败: {e}"))?;
        let decoder = BitmapDecoder::CreateAsync(&stream)
            .map_err(|e| format!("创建解码器失败: {e}"))?
            .get()
            .map_err(|e| format!("解码图片失败: {e}"))?;
        let bitmap = decoder
            .GetSoftwareBitmapAsync()
            .map_err(|e| format!("获取位图失败: {e}"))?
            .get()
            .map_err(|e| format!("等待位图失败: {e}"))?;

        // 2. OCR 引擎要求 Bgra8 + Premultiplied 格式，统一转换
        let bitmap = SoftwareBitmap::ConvertWithAlpha(
            &bitmap,
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
        )
        .map_err(|e| format!("位图格式转换失败: {e}"))?;

        // 3. 选择 OCR 引擎（zh 优先，可识别中英文混排）
        let engine = create_ocr_engine(language)?;

        // 4. 执行识别
        let result = engine
            .RecognizeAsync(&bitmap)
            .map_err(|e| format!("启动 OCR 失败: {e}"))?
            .get()
            .map_err(|e| format!("OCR 识别失败: {e}"))?;

        // 5. 收集结果（行矩形 = 所有词矩形的外包框）
        let mut lines = Vec::new();
        let lines_view = result
            .Lines()
            .map_err(|e| format!("获取识别结果失败: {e}"))?;
        for line in &lines_view {
            let text = line.Text().unwrap_or_default().to_string();
            if text.trim().is_empty() {
                continue;
            }
            let mut words = Vec::new();
            let mut min_x = f32::MAX;
            let mut min_y = f32::MAX;
            let mut max_r = f32::MIN;
            let mut max_b = f32::MIN;
            if let Ok(words_view) = line.Words() {
                for word in &words_view {
                    let wtext = word.Text().unwrap_or_default().to_string();
                    let r = word.BoundingRect().unwrap_or_default();
                    min_x = min_x.min(r.X);
                    min_y = min_y.min(r.Y);
                    max_r = max_r.max(r.X + r.Width);
                    max_b = max_b.max(r.Y + r.Height);
                    words.push(OcrWordInfo {
                        text: wtext,
                        x: r.X,
                        y: r.Y,
                        w: r.Width,
                        h: r.Height,
                    });
                }
            }
            let (x, y, w, h) = if min_x == f32::MAX {
                (0.0, 0.0, 0.0, 0.0)
            } else {
                (
                    min_x,
                    min_y,
                    (max_r - min_x).max(0.0),
                    (max_b - min_y).max(0.0),
                )
            };
            lines.push(OcrLineInfo {
                text,
                x,
                y,
                w,
                h,
                words,
            });
        }
        log::info!("OCR 完成: {} 行", lines.len());
        Ok(lines)
    }
}

/// 创建 OCR 引擎：指定语言标签时直接使用；否则自动选择
/// （zh-Hans > zh-Hant > zh-* > en-US > en-* > 第一个可用）
fn create_ocr_engine(language: Option<&str>) -> Result<windows::Media::Ocr::OcrEngine, String> {
    if let Some(tag) = language.filter(|s| !s.trim().is_empty()) {
        return create_engine_for_tag(tag);
    }
    create_preferred_engine()
}

/// 用指定 BCP-47 标签创建引擎
fn create_engine_for_tag(tag: &str) -> Result<windows::Media::Ocr::OcrEngine, String> {
    use windows::Globalization::Language;
    use windows::Media::Ocr::OcrEngine;

    unsafe {
        let lang = Language::CreateLanguage(&windows::core::HSTRING::from(tag))
            .map_err(|e| format!("创建语言对象失败: {e}"))?;
        OcrEngine::TryCreateFromLanguage(&lang)
            .map_err(|e| format!("创建 OCR 引擎失败（语言 {tag} 未安装?）: {e}"))
    }
}

/// 创建首选 OCR 引擎（zh-Hans > zh-Hant > zh-* > en-US > en-* > 第一个可用）
fn create_preferred_engine() -> Result<windows::Media::Ocr::OcrEngine, String> {
    use windows::Globalization::Language;
    use windows::Media::Ocr::OcrEngine;

    unsafe {
        let available = OcrEngine::AvailableRecognizerLanguages()
            .map_err(|e| format!("查询 OCR 语言失败: {e}"))?;
        let mut tags: Vec<String> = Vec::new();
        for l in &available {
            if let Ok(t) = l.LanguageTag() {
                tags.push(t.to_string());
            }
        }
        if tags.is_empty() {
            return Err("系统未安装任何 OCR 语言包（设置 → 时间和语言 → 语言 → 添加语言）".into());
        }
        log::info!("可用 OCR 语言: {:?}", tags);

        let pick = tags
            .iter()
            .find(|t| t.eq_ignore_ascii_case("zh-Hans"))
            .or_else(|| tags.iter().find(|t| t.eq_ignore_ascii_case("zh-Hant")))
            .or_else(|| tags.iter().find(|t| t.to_lowercase().starts_with("zh")))
            .or_else(|| tags.iter().find(|t| t.eq_ignore_ascii_case("en-US")))
            .or_else(|| tags.iter().find(|t| t.to_lowercase().starts_with("en")))
            .unwrap_or(&tags[0]);

        log::info!("使用 OCR 语言: {}", pick);
        let lang = Language::CreateLanguage(&windows::core::HSTRING::from(pick.as_str()))
            .map_err(|e| format!("创建语言对象失败: {e}"))?;
        OcrEngine::TryCreateFromLanguage(&lang).map_err(|e| format!("创建 OCR 引擎失败: {e}"))
    }
}
