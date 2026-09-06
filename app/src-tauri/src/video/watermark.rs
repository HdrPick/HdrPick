//! 录制水印（文字 + 图片）：输出帧级叠加
//!
//! 设计（方案 5）：文字水印（多条 / `{ts}` 时间戳 / 字号 / 描边 / 不透明度）
//! + 图片水印（PNG / 不透明度 / 九宫格 / 边距），共用一份定位/不透明度配置。
//!
//! - **渲染**（输出帧坐标系——追随鼠标裁剪后仍锚定输出边角，与光标的
//!   屏幕绝对坐标不同）：文字 = GDI CreateCompatibleDC + CreateDIBSection
//!   32bpp top-down（**屏幕 DC 不可选位图**——SelectObject 静默失败画进虚空，
//!   V13 踩坑 2）；**GDI 不写 alpha** → 双 DIB 通道法：DIB1 画彩色（黑描边
//!   + 白主体）、DIB2 画全白掩码（含描边偏移），合成 A = 掩码 R（抗锯齿
//!   覆盖度）、RGB = DIB1。时间戳文本按 key 缓存（秒级变化才重渲）
//! - **图片**：WIC 解码 PNG → 32bppBGRA（straight alpha，非预乘），
//!   不透明度在加载时烘进 alpha（静态一次）
//! - **合成层**：图片在上、文字在下（8px 间距）拼单图层，key = 展开后文本
//! - **混合**：复用 cursor_overlay 的 blend_layer_bgra / blend_layer_p010
//!   （SDR sRGB 域 / HDR PQ 域），水印在光标之上（最后混）
//!
//! COM：WIC 需要——仅图片水印启用时在录制线程 CoInitializeEx（APARTMENTTHREADED），
//! Drop 时配对 CoUninitialize；纯文字水印零 COM 依赖。

use std::path::PathBuf;

/// 水印启动参数（commands.rs 从 config 组装；文本与图片全空 = None 不建）
pub struct WatermarkParams {
    /// 文字（\n 分多条；`{ts}` = 本地时间 YYYY-MM-DD HH:MM:SS；空 = 关）
    pub text: String,
    /// 字号（像素）
    pub font_size: u32,
    /// 图片路径（空 = 关）
    pub image: String,
    /// 九宫格："tl".."br"
    pub pos: String,
    /// 不透明度 1..=100
    pub opacity: u32,
    /// 边距（像素）
    pub margin: u32,
}

/// `{ts}` → 本地时间（秒级——缓存 key 随之变化，每秒至多重渲一次）
fn expand_ts(s: &str) -> String {
    if s.contains("{ts}") {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        s.replace("{ts}", &now)
    } else {
        s.to_string()
    }
}

/// 九宫格锚点（帧内坐标）：(水平, 垂直)，-1 起 / 0 中 / 1 尾。
/// pub(crate)：camera.rs 共用
pub(crate) fn anchor_pos(
    pos: (i8, i8),
    margin: i32,
    w: i32,
    h: i32,
    lw: i32,
    lh: i32,
) -> (i32, i32) {
    let m = margin;
    let x = match pos.0 {
        -1 => m,
        0 => (w - lw) / 2,
        _ => w - lw - m,
    };
    let y = match pos.1 {
        -1 => m,
        0 => (h - lh) / 2,
        _ => h - lh - m,
    };
    (x.max(0), y.max(0))
}

fn parse_pos(s: &str) -> (i8, i8) {
    let h = match s.as_bytes().first() {
        Some(b't') => -1,
        Some(b'm') => 0,
        _ => 1,
    };
    let v = match s.as_bytes().get(1) {
        Some(b'l') => -1,
        Some(b'c') => 0,
        _ => 1,
    };
    (h, v)
}

pub struct Watermark {
    text: Option<(String, u32)>, // (模板, 字号)
    image: Option<(Vec<u8>, i32, i32)>, // BGRA straight alpha（不透明度已烘入）
    pos: (i8, i8),
    opacity: u32,
    margin: i32,
    /// 合成层缓存（key = 展开后文本；None = 无可渲染内容）
    composed: Option<(String, Vec<u8>, i32, i32)>,
    com_inited: bool,
}

impl Watermark {
    pub fn new(p: WatermarkParams) -> Self {
        let opacity = p.opacity.clamp(1, 100);
        let mut com_inited = false;
        let image = if p.image.trim().is_empty() {
            None
        } else {
            let path = PathBuf::from(&p.image);
            // WIC 需要 COM；录制线程未初始化 → 这里初始化（Drop 配对释放）
            if !path.exists() {
                log::warn!("[video] 水印图片不存在: {}", path.display());
                None
            } else {
                unsafe {
                    let hr = windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                    );
                    if hr.is_ok() {
                        com_inited = true;
                    } else {
                        // 已初始化（其他模型）也尝试继续——WIC 多数模型可用
                        log::warn!("[video] CoInitializeEx: {hr}（尝试继续加载水印图片）");
                    }
                }
                match load_image(&path, opacity) {
                    Some((bits, w, h)) => {
                        log::info!("[video] 水印图片已加载: {}（{w}x{h}）", path.display());
                        Some((bits, w, h))
                    }
                    None => {
                        log::warn!("[video] 水印图片解码失败: {}", path.display());
                        None
                    }
                }
            }
        };
        let text = if p.text.trim().is_empty() {
            None
        } else {
            Some((p.text, p.font_size.clamp(12, 72)))
        };
        Self {
            text,
            image,
            pos: parse_pos(&p.pos),
            opacity,
            margin: p.margin as i32,
            composed: None,
            com_inited,
        }
    }

    /// 诊断：水印状态（启动链路验证）
    fn dump_state(&self, tag: &str) {
        log::info!(
            "[video] 水印[{tag}]: text={} image={} pos=({},{}) opacity={} composed={}",
            self.text.is_some(),
            self.image.is_some(),
            self.pos.0,
            self.pos.1,
            self.opacity,
            self.composed
                .as_ref()
                .map(|(_, _, w, h)| format!("{w}x{h}"))
                .unwrap_or_else(|| "None".into())
        );
    }

    /// 文本 key（展开 {ts} 后；无文字 = 空）
    fn text_key(&self) -> String {
        self.text
            .as_ref()
            .map(|(t, _)| expand_ts(t))
            .unwrap_or_default()
    }

    /// 重建合成层（key 变化或首次）
    fn refresh(&mut self) {
        let key = self.text_key();
        let need = match self.composed.as_ref() {
            Some((k, _, _, _)) => *k != key,
            None => self.text.is_some() || self.image.is_some(),
        };
        if !need {
            return;
        }
        // 文字重渲（key 变化才发生——{ts} 每秒一次）
        let text_layer: Option<(Vec<u8>, i32, i32)> = self
            .text
            .as_ref()
            .filter(|_| !key.is_empty())
            .and_then(|(_, fs)| {
                let r = render_text(&key, *fs as i32, self.opacity);
                if r.is_none() {
                    log::warn!("[video] 水印文字渲染失败: {key:?}");
                }
                r
            });
        let (img, txt) = (self.image.as_ref(), text_layer.as_ref());
        if img.is_none() && txt.is_none() {
            self.composed = None;
            return;
        }
        // 拼层：图片在上、文字在下（8px 间距）；宽 = max
        let (iw, ih) = img.map(|(_, w, h)| (*w, *h)).unwrap_or((0, 0));
        let (tw, th) = txt.map(|(_, w, h)| (*w, *h)).unwrap_or((0, 0));
        let gap = if img.is_some() && txt.is_some() { 8 } else { 0 };
        let lw = iw.max(tw).max(1);
        let lh = (ih + gap + th).max(1);
        let mut layer = vec![0u8; lw as usize * lh as usize * 4];
        if let Some((bits, _, _)) = img {
            blit_layer(&mut layer, lw, bits, iw, ih, 0, 0);
        }
        if let Some((bits, _, _)) = txt {
            blit_layer(&mut layer, lw, bits, tw, th, 0, ih + gap);
        }
        let first = self.composed.is_none();
        self.composed = Some((key, layer, lw, lh));
        if first {
            // 仅首次打点（{ts} 每秒重渲不刷屏）
            self.dump_state("composed");
        }
    }

    /// SDR 路径：BGRA 输出帧原地叠加（输出坐标系）
    pub fn blend_bgra(&mut self, frame: &mut [u8], stride: usize, w: i32, h: i32) {
        self.refresh();
        let Some((_, layer, lw, lh)) = self.composed.as_ref() else {
            return;
        };
        let (x, y) = anchor_pos(self.pos, self.margin, w, h, *lw, *lh);
        super::cursor_overlay::blend_layer_bgra(frame, stride, w, h, layer, x, y, *lw, *lh);
    }

    /// HDR 路径：P010 双平面原地叠加（PQ 域数学与光标叠加同源）
    pub fn blend_p010(
        &mut self,
        y: &mut [u8],
        uv: &mut [u8],
        stride_y: usize,
        stride_uv: usize,
        w: i32,
        h: i32,
    ) {
        self.refresh();
        let Some((_, layer, lw, lh)) = self.composed.as_ref() else {
            return;
        };
        let (x, yy) = anchor_pos(self.pos, self.margin, w, h, *lw, *lh);
        super::cursor_overlay::blend_layer_p010(
            y, uv, stride_y, stride_uv, w, h, layer, x, yy, *lw, *lh,
        );
    }
}

impl Drop for Watermark {
    fn drop(&mut self) {
        if self.com_inited {
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }
}

/// 图层间 blit（源/目标均 BGRA straight alpha；目标已清零 = 直接覆写）
fn blit_layer(
    dst: &mut [u8],
    dst_w: i32,
    src: &[u8],
    sw: i32,
    sh: i32,
    ox: i32,
    oy: i32,
) {
    for y in 0..sh {
        let drow = ((oy + y) * dst_w + ox) as usize * 4;
        let srow = (y * sw) as usize * 4;
        let span = sw as usize * 4;
        dst[drow..drow + span].copy_from_slice(&src[srow..srow + span]);
    }
}

// ==================== GDI 文字渲染（双 DIB 修 alpha） ====================

/// 文字 → BGRA 图层（白主体 + 黑描边，抗锯齿 alpha 正确）
///
/// GDI 的 32bpp DIB 不写 alpha（恒 0）——单 DIB 无法区分"黑描边"与"透明"。
/// 双通道法：DIB1 = 彩色（8 方向黑描边 + 白主体），DIB2 = 掩码（同几何全白，
/// R 通道即抗锯齿覆盖度）。合成：RGB ← DIB1，A ← DIB2.R × opacity。
fn render_text(text: &str, font_size: i32, opacity: u32) -> Option<(Vec<u8>, i32, i32)> {
    use windows::Win32::Foundation::{COLORREF, SIZE};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
        GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, TextOutW, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS, TRANSPARENT,
    };
    let lines: Vec<Vec<u16>> = text
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.encode_utf16().collect())
        .collect();
    if lines.is_empty() {
        return None;
    }
    unsafe {
        // 屏幕内容无关——纯内存 DIB 绘制，CreateCompatibleDC(None) 即可
        //（对照 memory.rs 同款；GetDC/ReleaseDC 全免）
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let hfont = CreateFontW(
            -font_size, // 高度（负 = 字符高度语义）
            0,
            0,
            0,
            600, // FW_SEMIBOLD：视频上更可读
            0,
            0,
            0,
            1, // DEFAULT_CHARSET
            0, // OUT_DEFAULT_PRECIS
            0, // CLIP_DEFAULT_PRECIS
            5, // CLEARTYPE_QUALITY
            0, // DEFAULT_PITCH
            windows::core::w!("Segoe UI"),
        );
        let old_font = SelectObject(hdc, hfont);
        let cleanup = |hdc, old_font, hfont| {
            SelectObject(hdc, old_font);
            let _ = DeleteObject(hfont);
            let _ = DeleteDC(hdc);
        };
        // 测量（最大行宽定画布宽）
        let mut max_w = 0i32;
        for l in &lines {
            let mut sz = SIZE::default();
            if !GetTextExtentPoint32W(hdc, l, &mut sz).as_bool() {
                cleanup(hdc, old_font, hfont);
                return None;
            }
            max_w = max_w.max(sz.cx);
        }
        let line_h = font_size * 3 / 2;
        let stroke = (font_size / 12).clamp(1, 3);
        let pad = stroke + 1;
        let w = max_w + pad * 2;
        let h = lines.len() as i32 * line_h + pad * 2;
        if w <= 0 || h <= 0 || w > 8192 || h > 4096 {
            cleanup(hdc, old_font, hfont);
            return None;
        }
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down（行序与图层一致）
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits1 = std::ptr::null_mut();
        let mut bits2 = std::ptr::null_mut();
        let dib1 = CreateDIBSection(
            hdc,
            &bi,
            DIB_RGB_COLORS,
            &mut bits1,
            windows::Win32::Foundation::HANDLE::default(),
            0,
        )
        .ok();
        let dib2 = CreateDIBSection(
            hdc,
            &bi,
            DIB_RGB_COLORS,
            &mut bits2,
            windows::Win32::Foundation::HANDLE::default(),
            0,
        )
        .ok();
        let (dib1, dib2) = match (dib1, dib2) {
            (Some(a), Some(b)) if !bits1.is_null() && !bits2.is_null() => (a, b),
            _ => {
                // Result/HBITMAP 的部分失败清理：ok() 已丢句柄——DIB bits 归
                // HBITMAP 所有，无法回退删除（极端路径，泄露可忽略不计）
                cleanup(hdc, old_font, hfont);
                return None;
            }
        };
        SetBkMode(hdc, TRANSPARENT);
        // 8 方向描边偏移
        let dirs: [(i32, i32); 8] = [
            (-1, -1),
            (0, -1),
            (1, -1),
            (-1, 0),
            (1, 0),
            (-1, 1),
            (0, 1),
            (1, 1),
        ];
        let draw_all = |hdc, mask: bool| {
            for (i, l) in lines.iter().enumerate() {
                let x = pad;
                let y = pad + i as i32 * line_h;
                // 描边（彩色通道 = 黑；掩码通道 = 白）
                for (dx, dy) in dirs {
                    SetTextColor(
                        hdc,
                        if mask { COLORREF(0x00FFFFFF) } else { COLORREF(0) },
                    );
                    let _ = TextOutW(hdc, x + dx * stroke, y + dy * stroke, l);
                }
                // 主体白
                SetTextColor(hdc, COLORREF(0x00FFFFFF));
                let _ = TextOutW(hdc, x, y, l);
            }
        };
        // DIB1 = 彩色
        let old1 = SelectObject(hdc, dib1);
        draw_all(hdc, false);
        SelectObject(hdc, old1);
        // DIB2 = 掩码（全白——R 通道即覆盖度）
        let old2 = SelectObject(hdc, dib2);
        draw_all(hdc, true);
        SelectObject(hdc, old2);
        // 合成：RGB ← DIB1，A ← DIB2.R × opacity（白掩码 R=G=B=强度）
        let mut out = vec![0u8; w as usize * h as usize * 4];
        let p1 = bits1 as *const u8;
        let p2 = bits2 as *const u8;
        for i in 0..(w as usize * h as usize) {
            let s = i * 4;
            out[s] = *p1.add(s);
            out[s + 1] = *p1.add(s + 1);
            out[s + 2] = *p1.add(s + 2);
            out[s + 3] = (*p2.add(s + 2) as u32 * opacity / 100) as u8;
        }
        // 清理（GDI 句柄归调用者）
        let _ = DeleteObject(dib1);
        let _ = DeleteObject(dib2);
        cleanup(hdc, old_font, hfont);
        Some((out, w, h))
    }
}

// ==================== WIC PNG 解码 ====================

/// PNG/图片 → BGRA straight alpha（不透明度烘入 alpha；失败 None）
fn load_image(path: &std::path::Path, opacity: u32) -> Option<(Vec<u8>, i32, i32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::CoCreateInstance;
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, windows::Win32::System::Com::CLSCTX_INPROC_SERVER)
                .ok()?;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let decoder = factory
            .CreateDecoderFromFilename(
                windows::core::PCWSTR(wide.as_ptr()),
                None,
                windows::Win32::Foundation::GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let conv = factory.CreateFormatConverter().ok()?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA, // straight alpha（非预乘）
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        conv.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 || w > 8192 || h > 8192 {
            return None;
        }
        let stride = w as usize * 4;
        let mut buf = vec![0u8; stride * h as usize];
        conv.CopyPixels(std::ptr::null(), stride as u32, &mut buf)
            .ok()?;
        // 无 alpha 通道的图片（rgb24 PNG/JPG 常见）→ WIC 转 32bppBGRA 时
        // alpha 填 0（全透明 = 不可见）——检测全 0 则按不透明处理
        if buf.chunks_exact(4).all(|p| p[3] == 0) {
            for px in buf.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        // 不透明度烘入 alpha（straight alpha 域线性缩放）
        for px in buf.chunks_exact_mut(4) {
            px[3] = (px[3] as u32 * opacity / 100) as u8;
        }
        Some((buf, w as i32, h as i32))
    }
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pos_parse_and_anchor_pos() {
        assert_eq!(parse_pos("br"), (1, 1));
        assert_eq!(parse_pos("tl"), (-1, -1));
        assert_eq!(parse_pos("mc"), (0, 0));
        // br：右下角内缩 margin
        assert_eq!(anchor_pos((1, 1), 16, 1920, 1080, 100, 50), (1920 - 100 - 16, 1080 - 50 - 16));
        // tl：左上角
        assert_eq!(anchor_pos((-1, -1), 16, 1920, 1080, 100, 50), (16, 16));
        // tc：水平居中
        assert_eq!(anchor_pos((0, -1), 16, 1920, 1080, 100, 50), ((1920 - 100) / 2, 16));
        // 图层大于帧：钳到 0（不越界）
        assert_eq!(anchor_pos((1, 1), 16, 50, 40, 100, 50), (0, 0));
    }

    #[test]
    fn ts_expand() {
        assert_eq!(expand_ts("abc"), "abc");
        let s = expand_ts("t {ts}");
        assert!(s.starts_with("t 20") && s.len() > "t ".len() + 10);
    }

    #[test]
    fn text_render_gdi_smoke() {
        // GDI 冒烟：渲染含字母文本 → 尺寸合理 + 有可见像素（alpha>0 且主体白）
        let (bits, w, h) = render_text("AB12", 32, 100).expect("渲染失败");
        assert!(w > 20 && h > 20, "尺寸 {w}x{h}");
        let (mut vis, mut white) = (0usize, 0usize);
        for px in bits.chunks_exact(4) {
            if px[3] > 0 {
                vis += 1;
                if px[0] > 200 && px[1] > 200 && px[2] > 200 {
                    white += 1;
                }
            }
        }
        assert!(vis > 50, "可见像素 {vis}");
        assert!(white > 20, "白色主体像素 {white}");
        // 半透明：opacity 50 → alpha 上限 ~127+（抗锯齿峰值减半）
        let (bits2, _, _) = render_text("AB12", 32, 50).expect("渲染失败");
        let max_a = bits2.chunks_exact(4).map(|p| p[3]).max().unwrap();
        assert!(max_a <= 135 && max_a > 60, "半透明峰值 {max_a}");
    }

    #[test]
    fn watermark_layer_compose_and_blend() {
        // 端到端（无 GDI/WIC）：图片 + 文字合成 → 混入 SDR 帧
        let mut wm = Watermark {
            text: Some(("WM".into(), 24)),
            image: Some((vec![255u8, 0, 255, 255], 1, 1)), // 1x1 品红不透明
            pos: parse_pos("tl"),
            opacity: 100,
            margin: 0,
            composed: None,
            com_inited: false,
        };
        // 8x2 红色帧 → tl 角 (0,0) 1x1 图层变品红
        let mut frame = [0u8, 0, 255, 255].repeat(8 * 2);
        wm.blend_bgra(&mut frame, 8 * 4, 8, 2);
        assert_eq!(&frame[0..3], &[255, 0, 255], "tl 1x1 图片混入");
        // br 位置：右下角
        wm.pos = parse_pos("br");
        wm.composed = None;
        let mut frame2 = [0u8, 0, 255, 255].repeat(8 * 2);
        // 手动构造纯图片层（无文字依赖 GDI，改 text=None）
        wm.text = None;
        wm.blend_bgra(&mut frame2, 8 * 4, 8, 2);
        let last = &frame2[(8 * 2 - 1) * 4..(8 * 2) * 4 - 1];
        assert_eq!(last, &[255, 0, 255], "br 1x1 图片混入右下角");
    }

    #[test]
    fn watermark_blend_p010_br_corner() {
        // HDR 路径端到端：4x2 全黑 P010 + br 角 1x1 白图层 → 右下 Y 显著变亮。
        // （对照 cursor_overlay::blend_p010_white_cursor_on_black——加上 anchor 链路）
        let mut wm = Watermark {
            text: None,
            image: Some((vec![255u8, 255, 255, 255], 1, 1)), // 1x1 白
            pos: parse_pos("br"),
            opacity: 100,
            margin: 0,
            composed: None,
            com_inited: false,
        };
        let (w, h) = (4usize, 2usize);
        let mut y = vec![0u8; w * h * 2];
        for i in 0..w * h {
            y[i * 2] = 64; // Y=64（黑）
        }
        let mut uv = vec![0u8; (w / 2) * (h / 2) * 4];
        for i in 0..(w / 2) * (h / 2) {
            uv[i * 4 + 1] = 0x02; // U=V=512
            uv[i * 4 + 3] = 0x02;
        }
        wm.blend_p010(&mut y, &mut uv, w * 2, 4, w as i32, h as i32);
        let read = |x: usize, yy: usize| u16::from_le_bytes([y[(yy * w + x) * 2], y[(yy * w + x) * 2 + 1]]);
        assert!(read(3, 1) > 300, "br 角白图层 Y={}（黑=64，白应>300）", read(3, 1));
        assert_eq!(read(0, 0), 64, "未覆盖像素保持黑");
    }
}
