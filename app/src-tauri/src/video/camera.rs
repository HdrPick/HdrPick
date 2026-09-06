//! 摄像头画中画（Media Foundation 采集 + 色度键抠像 + 帧级合成）
//!
//! 设计（方案 5）：叠加到视频；位置九宫格、显示宽（高按摄像头宽高比）、
//! 水平翻转、色度键（颜色/相似度/软过渡）。另存独立 mp4 不做（P2 砍）。
//!
//! - **采集**：独立线程（CoInitializeEx MTA + MFStartup）——MFCreateDeviceSource
//!   （symbolic link 定位）→ IMFSourceReader 请求 RGB32（内部自动转）→
//!   ReadSample 同步循环 → 最新帧存共享槽（version 计数；录制线程按帧消费）
//! - **处理**（录制线程，version 变化才重算）：色度键（RGB 域阈值 + 软过渡带
//!   alpha 渐变）→ 双线性缩放到 PiP 尺寸（alpha 参与）→ 可选水平翻转
//! - **合成**：复用 cursor_overlay::blend_layer_bgra/p010（BGRA straight
//!   alpha 图层 + 九宫格 anchor 输出帧坐标系）；次序 = 光标 → 水印 → 摄像头
//!
//! Drop：停标志 → join 采集线程（线程内 source.Shutdown + MFShutdown +
//! CoUninitialize 配对释放）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// 摄像头设备描述（id = symbolic link——配置持久化的稳定标识）
pub struct CameraInfo {
    pub id: String,
    pub label: String,
}

/// 枚举视频捕获设备（命令层用；COM/MF 在本函数内配对初始化释放）
pub fn enumerate_cameras() -> Vec<CameraInfo> {
    unsafe {
        let hr = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
        let com_ok = hr.is_ok();
        if !com_ok && hr.0 != 0x8001_0106u32 as i32 {
            // RPC_E_CHANGED_MODE（线程已以其他模式初始化）也可继续——多数情况仍可用
            return Vec::new();
        }
        let mf = windows::Win32::Media::MediaFoundation::MFStartup(
            windows::Win32::Media::MediaFoundation::MF_VERSION,
            0,
        )
        .is_ok();
        let out = enumerate_inner();
        if mf {
            let _ = windows::Win32::Media::MediaFoundation::MFShutdown();
        }
        if com_ok {
            windows::Win32::System::Com::CoUninitialize();
        }
        out
    }
}

unsafe fn enumerate_inner() -> Vec<CameraInfo> {
    use windows::Win32::Media::MediaFoundation::*;
    let mut attrs: Option<IMFAttributes> = None;
    if MFCreateAttributes(&mut attrs, 2).is_err() {
        return Vec::new();
    }
    let attrs = match attrs {
        Some(a) => a,
        None => return Vec::new(),
    };
    if attrs
        .SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
        .is_err()
    {
        return Vec::new();
    }
    let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    if MFEnumDeviceSources(&attrs, &mut arr, &mut count).is_err() || arr.is_null() || count == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let items = std::slice::from_raw_parts(arr, count as usize);
    for it in items {
        let Some(act) = it else { continue };
        let get = |g: windows::core::GUID| -> Option<String> {
            let mut ws = windows::core::PWSTR::null();
            let mut len = 0u32;
            if act.GetAllocatedString(&g, &mut ws, &mut len).is_ok() && !ws.is_null() {
                let s = ws.to_string().ok()?;
                windows::Win32::System::Com::CoTaskMemFree(Some(ws.as_ptr() as _));
                return Some(s);
            }
            None
        };
        let Some(id) = get(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK) else {
            continue;
        };
        let label = get(MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_else(|| id.clone());
        out.push(CameraInfo { id, label });
    }
    windows::Win32::System::Com::CoTaskMemFree(Some(arr as _));
    out
}

// ==================== 采集会话 ====================

/// 摄像头启动参数（commands.rs 从 config 组装）
pub struct CameraParams {
    /// symbolic link（空 = 不启用）
    pub device: String,
    /// PiP 显示宽（像素；高 = 宽 × 摄像头宽高比，均取偶）
    pub width: u32,
    /// 九宫格 "tl".."br"
    pub pos: String,
    /// 水平翻转（前置摄像头镜像习惯）
    pub flip_h: bool,
    /// 色度键开关
    pub chroma_key: bool,
    /// 键控颜色 "#RRGGBB"
    pub key_color: String,
    /// 相似度（0..=100 → 阈值 0..=255）
    pub similarity: u32,
    /// 边距（像素）
    pub margin: u32,
}

/// 共享帧槽（采集线程写 → 录制线程读；version 单调递增）
struct SharedFrame {
    frame: Mutex<Option<(Vec<u8>, u32, u32)>>, // BGRA + 宽 + 高（步长 = 宽×4）
    version: AtomicU64,
}

pub struct Camera {
    shared: std::sync::Arc<SharedFrame>,
    stop: std::sync::Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    // 处理缓存（录制线程私有）
    processed_ver: u64,
    layer: Option<(Vec<u8>, i32, i32)>, // 处理后 BGRA 图层
    anchor: (i8, i8),
    margin: i32,
    /// PiP 显示宽（像素；0 = 自适应 = 输出宽 1/4）
    width: u32,
    flip_h: bool,
    chroma: Option<([u8; 3], i32)>, // 键控色 + 阈值（含软过渡起点）
}

impl Camera {
    /// 启动采集（设备打开失败返回 None + 日志——录制不受影响继续）
    pub fn new(p: CameraParams) -> Option<Self> {
        if p.device.trim().is_empty() {
            return None;
        }
        let key = parse_hex_color(&p.key_color);
        let sim = p.similarity.clamp(0, 100) as i32;
        let anchor = parse_pos(&p.pos);
        let shared = std::sync::Arc::new(SharedFrame {
            frame: Mutex::new(None),
            version: AtomicU64::new(0),
        });
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let device = p.device.clone();
        let shared2 = std::sync::Arc::clone(&shared);
        let stop2 = std::sync::Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("video-camera".into())
            .spawn(move || capture_thread(&device, &shared2, &stop2))
            .ok()?;
        log::info!("[video] 摄像头画中画已启动: {}", p.device);
        Some(Self {
            shared,
            stop,
            handle: Some(handle),
            processed_ver: 0,
            layer: None,
            anchor,
            margin: p.margin as i32,
            width: p.width,
            flip_h: p.flip_h,
            chroma: if p.chroma_key && key.is_some() {
                key.map(|k| (k, sim * 255 / 100))
            } else {
                None
            },
        })
    }

    /// 处理最新帧（version 变化才重算：色度键 → 缩放 → 翻转）
    /// out_w = 输出帧宽（width=0 自适应时取 1/4）
    fn refresh_layer(&mut self, out_w: i32) -> bool {
        let (ver, raw) = {
            let g = self.shared.frame.lock().unwrap_or_else(|p| p.into_inner());
            match g.as_ref() {
                Some((d, w, h)) => (
                    self.shared.version.load(Ordering::Relaxed),
                    (d.clone(), *w, *h),
                ),
                None => return false,
            }
        };
        if ver == 0 {
            return false;
        }
        if ver == self.processed_ver && self.layer.is_some() {
            return true;
        }
        let (data, cw, ch) = raw;
        let (sw, sh) = (cw as i32, ch as i32);
        // PiP 宽：配置显式值；0 = 自适应（输出宽 1/4，如 2560 → 640）
        let pip_w = if self.width > 0 {
            self.width as i32
        } else {
            out_w / 4
        };
        // PiP 尺寸：宽 = 配置（偶数），高 = 宽 × 摄像头宽高比（偶数，≥2）
        let dw = pip_w.max(16) & !1;
        let dh = ((dw as i64 * sh as i64 / sw as i64).max(2) & !1) as i32;
        let layer = process_frame(&data, sw, sh, dw, dh, self.flip_h, self.chroma);
        self.layer = Some((layer, dw, dh));
        self.processed_ver = ver;
        true
    }

    /// SDR：BGRA 输出帧合成（输出坐标系九宫格锚定）
    pub fn blend_bgra(&mut self, frame: &mut [u8], stride: usize, w: i32, h: i32) {
        if !self.refresh_layer(w) {
            return;
        }
        let Some((layer, lw, lh)) = self.layer.as_ref() else {
            return;
        };
        let (x, y) = super::watermark::anchor_pos(self.anchor, self.margin, w, h, *lw, *lh);
        super::cursor_overlay::blend_layer_bgra(frame, stride, w, h, layer, x, y, *lw, *lh);
    }

    /// HDR：P010 双平面合成（PQ 域同源）
    pub fn blend_p010(
        &mut self,
        y: &mut [u8],
        uv: &mut [u8],
        stride_y: usize,
        stride_uv: usize,
        w: i32,
        h: i32,
    ) {
        if !self.refresh_layer(w) {
            return;
        }
        let Some((layer, lw, lh)) = self.layer.as_ref() else {
            return;
        };
        let (x, yy) = super::watermark::anchor_pos(self.anchor, self.margin, w, h, *lw, *lh);
        super::cursor_overlay::blend_layer_p010(
            y, uv, stride_y, stride_uv, w, h, layer, x, yy, *lw, *lh,
        );
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ==================== 采集线程 ====================

fn capture_thread(device: &str, shared: &SharedFrame, stop: &AtomicBool) {
    unsafe {
        let hr = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
        let com_ok = hr.is_ok();
        if !com_ok && hr.0 != 0x8001_0106u32 as i32 {
            log::warn!("[video] 摄像头线程 CoInitializeEx: {hr}");
            return;
        }
        if let Err(e) = windows::Win32::Media::MediaFoundation::MFStartup(
            windows::Win32::Media::MediaFoundation::MF_VERSION,
            0,
        ) {
            log::warn!("[video] MFStartup: {e}");
            if com_ok {
                windows::Win32::System::Com::CoUninitialize();
            }
            return;
        }
        capture_loop(device, shared, stop);
        let _ = windows::Win32::Media::MediaFoundation::MFShutdown();
        if com_ok {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}

unsafe fn capture_loop(device: &str, shared: &SharedFrame, stop: &AtomicBool) {
    use windows::Win32::Media::MediaFoundation::*;
    // 设备定位属性（symbolic link）
    let mut attrs: Option<IMFAttributes> = None;
    if let Err(e) = MFCreateAttributes(&mut attrs, 2) {
        log::warn!("[video] 摄像头 MFCreateAttributes: {e}");
        return;
    }
    let Some(attrs) = attrs else { return };
    let dev_w: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
    if let Err(e) = attrs.SetGUID(
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    ) {
        log::warn!("[video] 摄像头属性: {e}");
        return;
    }
    if let Err(e) = attrs.SetString(
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        windows::core::PCWSTR(dev_w.as_ptr()),
    ) {
        log::warn!("[video] 摄像头设备属性: {e}");
        return;
    }
    let source = match MFCreateDeviceSource(&attrs) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("[video] 摄像头打开失败: {e}");
            return;
        }
    };
    // Reader 属性：启用视频处理（MJPG → RGB32 转换必需——默认关闭，
    // SetCurrentMediaType 会拒绝 subtype 变更，V14 实测）
    let mut rattrs: Option<IMFAttributes> = None;
    let reader = match (|| -> windows::core::Result<IMFSourceReader> {
        MFCreateAttributes(&mut rattrs, 1)?;
        let Some(ra) = rattrs.as_ref() else {
            return Err(windows::core::Error::from_win32());
        };
        ra.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        MFCreateSourceReaderFromMediaSource(&source, ra)
    })() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[video] SourceReader 创建失败: {e}");
            let _ = source.Shutdown();
            return;
        }
    };
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let _ = reader.SetStreamSelection(stream, true);
    // 请求 RGB32：复制原生类型仅换 subtype（reader 自动插入解码器 + 颜色转换
    // 器——空类型只设 major/subtype 在 MJPG 设备上会失败，V14 实测）
    if let Ok(native) = reader.GetCurrentMediaType(stream) {
        if let Ok(mt) = MFCreateMediaType() {
            if native.CopyAllItems(&mt).is_ok()
                && mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).is_ok()
                && reader.SetCurrentMediaType(stream, None, &mt).is_err()
            {
                log::warn!("[video] 摄像头 RGB32 转换协商失败（尝试保留原生格式）");
            }
        }
    }
    // 帧尺寸 + 步长 + subtype 校验（非 RGB32 = 压缩/计划格式，BGRA 假设失效
    // → 明确报错退出，避免静默产出垃圾画面）
    let (cw, ch, stride) = match reader.GetCurrentMediaType(stream) {
        Ok(t) => {
            let sub = t
                .GetGUID(&MF_MT_SUBTYPE)
                .unwrap_or(windows::core::GUID::zeroed());
            if sub != MFVideoFormat_RGB32 {
                log::warn!("[video] 摄像头当前输出非 RGB32（subtype 不符）——不支持软件转换，放弃 PiP");
                let _ = source.Shutdown();
                return;
            }
            let fs = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            let w = (fs >> 32) as u32;
            let h = (fs & 0xFFFF_FFFF) as u32;
            let stride = t.GetUINT64(&MF_MT_DEFAULT_STRIDE).unwrap_or(0) as i32;
            (w, h, if stride == 0 { w as i32 * 4 } else { stride })
        }
        Err(_) => (0, 0, 0),
    };
    if cw == 0 || ch == 0 || stride == 0 {
        log::warn!("[video] 摄像头媒体类型无效");
        let _ = source.Shutdown();
        return;
    }
    let bottom_up = stride < 0;
    let row_pitch = stride.unsigned_abs() as usize;
    log::info!("[video] 摄像头输出: {cw}x{ch} stride={stride} bottom_up={bottom_up}");
    while !stop.load(Ordering::Acquire) {
        let mut sample: Option<IMFSample> = None;
        let mut flags = 0u32;
        if let Err(e) =
            reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
        {
            log::warn!("[video] 摄像头 ReadSample: {e}");
            break;
        }
        if flags & 1 != 0 || flags & 2 != 0 {
            // ERROR / ENDOFSTREAM
            break;
        }
        let Some(s) = sample else {
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        };
        let Ok(buf) = s.ConvertToContiguousBuffer() else {
            continue;
        };
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        if buf.Lock(&mut ptr, None, Some(&mut len)).is_err() {
            continue;
        }
        let need = row_pitch * ch as usize;
        if len as usize >= need {
            let src = std::slice::from_raw_parts(ptr, need);
            // 负步长 = bottom-up DIB → 行序翻转归一 top-down；紧排 BGRA
            let mut out = vec![0u8; cw as usize * ch as usize * 4];
            for y in 0..ch as usize {
                let srow = if bottom_up { ch as usize - 1 - y } else { y };
                let d = y * cw as usize * 4;
                let s = srow * row_pitch;
                out[d..d + cw as usize * 4].copy_from_slice(&src[s..s + cw as usize * 4]);
            }
            // RGB32 的 X 通道 alpha 未定义 → 强制 255
            for px in out.chunks_exact_mut(4) {
                px[3] = 255;
            }
            let mut g = shared.frame.lock().unwrap_or_else(|p| p.into_inner());
            *g = Some((out, cw, ch));
            drop(g);
            shared.version.fetch_add(1, Ordering::Release);
        }
        let _ = buf.Unlock();
        // PiP 30fps 足够（省 CPU/USB 带宽）
        std::thread::sleep(std::time::Duration::from_millis(33));
    }
    let _ = source.Shutdown();
}

// ==================== 帧处理（色度键 + 缩放 + 翻转） ====================

/// 摄像头帧 → PiP 图层：色度键（含 20 级软过渡）→ 双线性缩放（alpha 参与）
/// → 可选水平翻转
fn process_frame(
    data: &[u8],
    sw: i32,
    sh: i32,
    dw: i32,
    dh: i32,
    flip_h: bool,
    chroma: Option<([u8; 3], i32)>,
) -> Vec<u8> {
    let mut keyed;
    let src: &[u8] = if let Some((k, t)) = chroma {
        keyed = data.to_vec();
        let soft = 20i32;
        for px in keyed.chunks_exact_mut(4) {
            let d = (px[2] as i32 - k[0] as i32)
                .abs()
                .max((px[1] as i32 - k[1] as i32).abs())
                .max((px[0] as i32 - k[2] as i32).abs());
            px[3] = if d <= t {
                0
            } else if d < t + soft {
                ((d - t) * 255 / soft).clamp(0, 255) as u8
            } else {
                255
            };
        }
        &keyed
    } else {
        data
    };
    // 双线性缩放（x 采样含翻转）
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
    let x_ratio = sw as f32 / dw as f32;
    let y_ratio = sh as f32 / dh as f32;
    for dy in 0..dh {
        let syf = (dy as f32 + 0.5) * y_ratio - 0.5;
        let sy0 = syf.floor().clamp(0.0, sh as f32 - 1.0) as i32;
        let sy1 = (sy0 + 1).min(sh - 1);
        let fy = (syf - sy0 as f32).clamp(0.0, 1.0);
        for dx in 0..dw {
            let sxf = (dx as f32 + 0.5) * x_ratio - 0.5;
            let sx0f = if flip_h {
                sw as f32 - 1.0 - sxf
            } else {
                sxf
            };
            let sx0 = sx0f.floor().clamp(0.0, sw as f32 - 1.0) as i32;
            let sx1 = (sx0 + 1).min(sw - 1);
            let fx = (sx0f - sx0 as f32).clamp(0.0, 1.0);
            let s = |x: i32, y: i32| -> [f32; 4] {
                let i = (y * sw + x) as usize * 4;
                [
                    src[i] as f32,
                    src[i + 1] as f32,
                    src[i + 2] as f32,
                    src[i + 3] as f32,
                ]
            };
            let a = s(sx0, sy0);
            let b = s(sx1, sy0);
            let c = s(sx0, sy1);
            let d2 = s(sx1, sy1);
            let d3 = (dy * dw + dx) as usize * 4;
            for c2 in 0..4 {
                let top = a[c2] * (1.0 - fx) + b[c2] * fx;
                let bot = c[c2] * (1.0 - fx) + d2[c2] * fx;
                out[d3 + c2] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// "#RRGGBB" → RGB。pub(crate)：commands.rs 高亮颜色解析共用
pub(crate) fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let t = s.trim().trim_start_matches('#');
    if t.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(t, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8]) // RGB
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

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_pos() {
        assert_eq!(parse_hex_color("#00FF80"), Some([0x00, 0xFF, 0x80]));
        assert_eq!(parse_hex_color("bad"), None);
        assert_eq!(parse_pos("br"), (1, 1));
        assert_eq!(parse_pos("tl"), (-1, -1));
    }

    #[test]
    fn chroma_key_thresholds() {
        // 2x1 帧：绿（键控色）+ 白
        let mut f = vec![0u8, 255, 0, 255, 255, 255, 255, 255];
        let out = process_frame(&mut f, 2, 1, 2, 1, false, Some(([0, 255, 0], 60)));
        assert_eq!(out[3], 0, "绿像素键控为透明");
        assert_eq!(out[7], 255, "白像素保持不透明");
        // 阈值 0：接近绿但不等（G 差 5，R/B=0）→ 软过渡带
        let mut f2 = vec![0u8, 250, 0, 255];
        let out2 = process_frame(&mut f2, 1, 1, 1, 1, false, Some(([0, 255, 0], 0)));
        // d = 5 → 软带内 (5/20)*255 ≈ 63
        assert!(out2[3] > 40 && out2[3] < 90, "软过渡 alpha={}", out2[3]);
    }

    #[test]
    fn scale_and_flip() {
        // 2x2 帧缩放到 4x2（x2 放大）：行值复制；翻转后左右镜像
        let f = vec![
            255, 0, 0, 255, 0, 255, 0, 255, //
            0, 0, 255, 255, 255, 255, 255, 255,
        ];
        // 不翻转：左上角红
        let out = process_frame(&f, 2, 2, 4, 2, false, None);
        assert_eq!(&out[0..3], &[255, 0, 0]);
        // 翻转：左上角变绿（原图右上）
        let out2 = process_frame(&f, 2, 2, 4, 2, true, None);
        assert_eq!(&out2[0..3], &[0, 255, 0]);
    }

    #[test]
    fn camera_blend_bgra_br_corner() {
        // 合成链路（无真实设备）：手动注入共享帧 → br 角白块
        let cam = Camera {
            shared: std::sync::Arc::new(SharedFrame {
                frame: Mutex::new(Some((vec![255u8; 4 * 2 * 2], 2, 2))),
                version: AtomicU64::new(1),
            }),
            stop: std::sync::Arc::new(AtomicBool::new(false)),
            handle: None,
            processed_ver: 0,
            layer: None,
            anchor: parse_pos("br"),
            margin: 0,
            width: 0,
            flip_h: false,
            chroma: None,
        };
        let mut cam = cam;
        // 80x20 帧：PiP 自适应宽 = 80/4 = 20（≥ 16px 下限），高 = 20（2x2 源等比）
        let (w, h) = (80usize, 20usize);
        let mut frame = [0u8, 0, 255, 255].repeat(w * h);
        cam.blend_bgra(&mut frame, w * 4, w as i32, h as i32);
        // br 角：x = 80-20 = 60 → 右侧 20 列（全高）变白
        let last = &frame[(w * h - 1) * 4..(w * h) * 4 - 1];
        assert_eq!(last, &[255, 255, 255], "br 角摄像头块混入");
        // 左上角保持红
        assert_eq!(&frame[0..3], &[0, 0, 255]);
        // PiP 左边界外一列（x=59，第 10 行）保持红
        let out_col = 10 * w * 4 + 59 * 4;
        assert_eq!(&frame[out_col..out_col + 3], &[0, 0, 255]);
        // PiP 区域内（x=70，第 10 行）变白
        let in_col = 10 * w * 4 + 70 * 4;
        assert_eq!(&frame[in_col..in_col + 3], &[255, 255, 255]);
    }

    /// 真实设备枚举打印（E2E 准备：-- --nocapture 运行拿 symbolic link；无断言）
    #[test]
    fn enumerate_real_print() {
        for c in enumerate_cameras() {
            println!("[camera] {} -> {}", c.label, c.id);
        }
    }

    /// 真机全链路：枚举首个设备 → Camera::new（MF 采集线程）→ 等帧 →
    /// 2560x1600 纯红帧 br 角合成 → PiP 区域出现非红像素。
    /// 覆盖：设备打开 / RGB32 读取 / 色度键+缩放+翻转 / blend_layer_bgra 混合
    ///（session.rs 四路接线与水印对称，编译期保证）。无摄像头环境跳过。
    #[test]
    fn real_device_blend_e2e() {
        let _ = env_logger::builder().is_test(true).try_init();
        let Some(cam_info) = enumerate_cameras().into_iter().next() else {
            println!("[camera-e2e] 无摄像头设备，跳过");
            return;
        };
        println!("[camera-e2e] 设备: {}", cam_info.label);
        let Some(mut cam) = Camera::new(CameraParams {
            device: cam_info.id,
            width: 320,
            pos: "br".to_string(),
            flip_h: true,
            chroma_key: false,
            key_color: "#00FF00".to_string(),
            similarity: 60,
            margin: 16,
        }) else {
            panic!("[camera-e2e] Camera::new 失败（设备枚举成功但打开失败）");
        };
        // 等采集线程稳定出帧（USB 初始化 + 首帧常为曝光黑帧——跳过前几帧）
        let mut got_frame = false;
        for _ in 0..80 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let ver = cam.shared.version.load(Ordering::Relaxed);
            if ver >= 5 {
                got_frame = true;
                println!("[camera-e2e] 采集线程稳定出帧（version={ver}）");
                break;
            }
        }
        assert!(got_frame, "8s 内无稳定摄像头帧（采集线程异常）");
        // 2560x1600 纯红帧（BGRA）合成
        let (w, h) = (2560usize, 1600usize);
        let mut frame = vec![0u8, 0, 255, 255].repeat(w * h);
        cam.blend_bgra(&mut frame, w * 4, w as i32, h as i32);
        // br 角 PiP 区域（x≥2224；PiP 320x240 @ y 1344..1584）应有非纯红像素
        //（摄像头画面——黑/任何非 (0,0,255) 内容都算混入成功）
        let (x0, y0) = (w - 320 - 16, h - 320 - 16);
        let mut non_red = 0usize;
        for y in (y0..h).step_by(4) {
            for x in (x0..w).step_by(4) {
                let i = (y * w + x) * 4;
                let is_pure_red = frame[i] < 40 && frame[i + 1] < 40 && frame[i + 2] > 200;
                if !is_pure_red {
                    non_red += 1;
                }
            }
        }
        println!("[camera-e2e] PiP 区域非红采样 {non_red} 点");
        assert!(
            non_red > 500,
            "PiP 区域摄像头画面不可见（非红 {non_red} 点）"
        );
        // Drop：采集线程停止 + join（验证优雅关闭无死锁——Drop 内部 join）
        drop(cam);
        println!("[camera-e2e] Drop 完成（采集线程已 join）");
    }
}
