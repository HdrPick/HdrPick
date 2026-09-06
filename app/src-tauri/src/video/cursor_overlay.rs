//! 鼠标光标 + 点击效果叠加（录制帧 CPU 后处理）
//!
//! DDA（DXGI Desktop Duplication）采集的帧**不含鼠标光标**（DWM 合成层不含
//! 指针位图）——教程类录制必须软件叠加。本模块在帧读回 staging 后、编码
//! push 前原地混合（staging 已改 MAP_READ_WRITE，零拷贝）：
//!
//! - **光标捕获**：GetCursorInfo + GetIconInfo → 彩色光标 GetDIBits 32bpp
//!   BGRA（straight alpha）；单色光标（hbmColor=NULL，如标准箭头）解码
//!   2h 高掩码位图（上半 AND / 下半 XOR，top-down 请求）
//! - **点击效果**：WH_MOUSE_LL 低级钩子（独立消息泵线程）记录左/右击时刻；
//!   渲染 0.5s 扩散圆环（左=青、右=红），半径增、透明度降
//! - **SDR 路径**：BGRA8 sRGB 域 straight-alpha 混合
//! - **HDR 路径**：P010 解码（10bit→YCbCr→PQ RGB→EOTF→线性→BT.2020→BT.709
//!   →sRGB）→ 混合 → 编码回 P010（矩阵/常数与 p010_gpu.rs shader 严格对偶，
//!   UV 按 2×2 PQ 域平均下采样重算——光标小区域开销可忽略）
//!
//! 光标图层（动态包围盒 BGRA 画布）统一承载光标位图 + 点击圈，blend 阶段
//! 一次遍历——两条路径共享图层合成逻辑。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::color::matrix::{bt2020_to_bt709, bt709_to_bt2020};

// ==================== 点击记录（钩子线程写 → 录制线程读） ====================

/// 单次点击事件
#[derive(Clone, Copy)]
struct ClickEvent {
    /// 0 = 左键 / 1 = 右键
    button: u8,
    x: i32,
    y: i32,
    at: Instant,
}

/// 点击环形槽（容量 16；钩子线程 push，录制线程 active() 顺带清理过期）
static CLICKS: Mutex<Vec<ClickEvent>> = Mutex::new(Vec::new());

/// 钩子安装标志（RING 回调只在安装后写 CLICKS——多录制会话互不干扰）
static HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

fn push_click(ev: ClickEvent) {
    if !HOOK_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut g) = CLICKS.lock() {
        if g.len() >= 16 {
            g.remove(0);
        }
        g.push(ev);
    }
    log::info!(
        "[video] 点击事件: button={} ({},{})",
        ev.button,
        ev.x,
        ev.y
    );
}

/// 活跃点击（window_ms 内；过期清理——点击圈 500ms / 高亮块 1000ms）
fn active_clicks(window_ms: u128) -> Vec<ClickEvent> {
    match CLICKS.lock() {
        Ok(mut g) => {
            g.retain(|e| e.at.elapsed().as_millis() < window_ms);
            g.clone()
        }
        Err(_) => Vec::new(),
    }
}

/// 点击圈动画时长（毫秒）
const CLICK_DURATION_MS: u128 = 500;

/// 高亮块动画时长（毫秒）
const HIGHLIGHT_DURATION_MS: u128 = 1000;

// ==================== 低级鼠标钩子（WH_MOUSE_LL 消息泵线程） ====================

unsafe extern "system" fn mouse_ll_proc(
    code: i32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, MSLLHOOKSTRUCT, WM_LBUTTONDOWN, WM_RBUTTONDOWN,
    };
    if code >= 0 {
        let msg = wp.0 as u32;
        if msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN {
            let st = &*(lp.0 as *const MSLLHOOKSTRUCT);
            push_click(ClickEvent {
                button: if msg == WM_RBUTTONDOWN { 1 } else { 0 },
                x: st.pt.x,
                y: st.pt.y,
                at: Instant::now(),
            });
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// 钩子线程句柄（Drop：PostThreadMessage WM_QUIT → 泵退出 → Unhook）
struct HookHandle {
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
        HOOK_ACTIVE.store(false, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 启动点击钩子线程（SetWindowsHookExW + GetMessageW 泵；WM_QUIT 退出并卸载）
fn spawn_click_hook() -> Option<HookHandle> {
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    let join = std::thread::Builder::new()
        .name("video-click-hook".into())
        .spawn(move || unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG, WH_MOUSE_LL,
            };
            let tid = windows::Win32::System::Threading::GetCurrentThreadId();
            let _ = tx.send(tid);
            let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), None, 0) {
                Ok(h) => h,
                Err(_) => return,
            };
            HOOK_ACTIVE.store(true, Ordering::Release);
            log::info!("[video] 点击钩子已安装（tid={tid}）");
            let mut msg = MSG::default();
            loop {
                // GetMessage 返回 0（WM_QUIT）或 -1（错误）都退出
                let r = GetMessageW(&mut msg, None, 0, 0);
                if !r.as_bool() {
                    break;
                }
            }
            HOOK_ACTIVE.store(false, Ordering::Release);
            let _ = UnhookWindowsHookEx(hook);
        })
        .ok()?;
    let thread_id = rx.recv().ok()?;
    if thread_id == 0 {
        return None;
    }
    Some(HookHandle { thread_id, join: Some(join) })
}

// ==================== 叠加器主体 ====================

/// 图层最大边长（承载光标 + 最大扩散圈 + 最大高亮块；光标 ≤64 + 圈半径 ≤40
/// + 高亮 ≤300 → 联合包围盒上限 512）
const LAYER_MAX: i32 = 512;

pub struct CursorOverlay {
    cursor_enabled: bool,
    click_enabled: bool,
    /// 高亮效果：Some((RGB 色, 边长像素))——点击点为中心半透明色块 1s 渐隐
    highlight: Option<([u8; 3], i32)>,
    hook: Option<HookHandle>,
    // 光标位图缓存（HCURSOR 不变则复用——形状切换才重建）
    last_cursor: isize,
    cursor_bits: Vec<u8>, // BGRA straight alpha，top-down
    cursor_w: i32,
    cursor_h: i32,
    cursor_hot: (i32, i32),
}

impl CursorOverlay {
    pub fn new(cursor_enabled: bool, click_effect: bool, highlight: Option<([u8; 3], i32)>) -> Self {
        Self {
            cursor_enabled,
            click_enabled: click_effect,
            highlight,
            hook: None,
            last_cursor: 0,
            cursor_bits: Vec::new(),
            cursor_w: 0,
            cursor_h: 0,
            cursor_hot: (0, 0),
        }
    }

    /// 启动点击钩子（录制会话开始时调用一次）
    pub fn start(&mut self) {
        log::info!(
            "[video] 光标叠加启动: cursor={} click={} highlight={}",
            self.cursor_enabled,
            self.click_enabled,
            self.highlight.is_some()
        );
        if (self.click_enabled || self.highlight.is_some()) && self.hook.is_none() {
            self.hook = spawn_click_hook();
            if self.hook.is_none() {
                log::warn!("[video] 点击效果钩子启动失败（不影响录制）");
            }
        }
    }

    /// SDR 路径：BGRA8 sRGB 帧原地叠加（staging READ_WRITE 映射内存）
    pub fn blend_bgra(&mut self, frame: &mut [u8], stride: usize, w: i32, h: i32) {
        self.blend_bgra_off(frame, stride, w, h, 0, 0);
    }

    /// SDR 路径（追随鼠标裁剪帧）：off_x/off_y = 裁剪窗在显示器坐标系的原点——
    /// 图层按显示器绝对坐标构建，混合前平移 (lx−off_x, ly−off_y)
    pub fn blend_bgra_off(
        &mut self,
        frame: &mut [u8],
        stride: usize,
        w: i32,
        h: i32,
        off_x: i32,
        off_y: i32,
    ) {
        if let Some(((lx, ly, lw, lh), layer)) = self.build_layer() {
            blend_layer_bgra(frame, stride, w, h, &layer, lx - off_x, ly - off_y, lw, lh);
        }
    }

    /// HDR 路径：P010 双平面原地叠加（PQ 域数学与 p010_gpu.rs shader 对偶）
    pub fn blend_p010(
        &mut self,
        y: &mut [u8],
        uv: &mut [u8],
        stride_y: usize,
        stride_uv: usize,
        w: i32,
        h: i32,
    ) {
        self.blend_p010_off(y, uv, stride_y, stride_uv, w, h, 0, 0);
    }

    /// HDR 路径（追随鼠标裁剪帧；off 偶数对齐——UV 2×2 块）
    pub fn blend_p010_off(
        &mut self,
        y: &mut [u8],
        uv: &mut [u8],
        stride_y: usize,
        stride_uv: usize,
        w: i32,
        h: i32,
        off_x: i32,
        off_y: i32,
    ) {
        if let Some(((lx, ly, lw, lh), layer)) = self.build_layer() {
            blend_layer_p010(
                y,
                uv,
                stride_y,
                stride_uv,
                w,
                h,
                &layer,
                lx - off_x,
                ly - off_y,
                lw,
                lh,
            );
        }
    }

    /// 捕获光标屏幕位置（位图缓存按需重建）。None = 光标隐藏。
    fn capture_cursor(&mut self) -> Option<(i32, i32)> {
        unsafe {
            let mut ci = windows::Win32::UI::WindowsAndMessaging::CURSORINFO {
                cbSize: std::mem::size_of::<
                    windows::Win32::UI::WindowsAndMessaging::CURSORINFO,
                >() as u32,
                ..Default::default()
            };
            if windows::Win32::UI::WindowsAndMessaging::GetCursorInfo(&mut ci).is_err() {
                return None;
            }
            if ci.flags != windows::Win32::UI::WindowsAndMessaging::CURSOR_SHOWING {
                return None;
            }
            let hcur = ci.hCursor.0 as isize;
            if hcur != self.last_cursor || self.cursor_bits.is_empty() {
                let (bits, w, h, hot) = cursor_bitmap(ci.hCursor)?;
                self.cursor_bits = bits;
                self.cursor_w = w;
                self.cursor_h = h;
                self.cursor_hot = hot;
                self.last_cursor = hcur;
            }
            Some((ci.ptScreenPos.x, ci.ptScreenPos.y))
        }
    }

    /// 合成图层（光标位图 + 活跃点击圈 + 高亮块）。
    /// 返回 ((帧内偏移 x,y, 图层宽,高), 图层数据)；空场景 None（零开销跳过）。
    fn build_layer(&mut self) -> Option<((i32, i32, i32, i32), Vec<u8>)> {
        let mut cursor_pos: Option<(i32, i32)> = None; // 光标热点屏幕位置
        if self.cursor_enabled {
            cursor_pos = self.capture_cursor();
        }
        // 点击事件（点击圈和高亮共用：高亮渐隐期 1s > 圈 0.5s——按启用项取窗口）
        let need_clicks = self.click_enabled || self.highlight.is_some();
        let clicks: Vec<ClickEvent> = if need_clicks {
            active_clicks(if self.highlight.is_some() {
                HIGHLIGHT_DURATION_MS
            } else {
                CLICK_DURATION_MS
            })
        } else {
            Vec::new()
        };
        if cursor_pos.is_none() && clicks.is_empty() {
            return None;
        }

        // 内容包围盒
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        if let Some((px, py)) = cursor_pos {
            let cx = px - self.cursor_hot.0;
            let cy = py - self.cursor_hot.1;
            min_x = min_x.min(cx);
            min_y = min_y.min(cy);
            max_x = max_x.max(cx + self.cursor_w);
            max_y = max_y.max(cy + self.cursor_h);
        }
        for c in &clicks {
            let r = if self.click_enabled {
                click_radius(c) as i32
            } else {
                0
            };
            let hr = self.highlight.map(|(_, s)| s / 2).unwrap_or(0);
            let er = r.max(hr);
            min_x = min_x.min(c.x - er);
            min_y = min_y.min(c.y - er);
            max_x = max_x.max(c.x + er);
            max_y = max_y.max(c.y + er);
        }
        // 画布 = 包围盒（上限 LAYER_MAX：以首元素为锚裁剪）
        let lw = (max_x - min_x).clamp(1, LAYER_MAX);
        let lh = (max_y - min_y).clamp(1, LAYER_MAX);
        let mut layer = vec![0u8; lw as usize * lh as usize * 4];

        // 绘制次序（下→上）：高亮块 → 点击圈 → 光标（指针始终最清晰）
        if let Some((color, size)) = self.highlight {
            for c in &clicks {
                draw_highlight_block(
                    &mut layer,
                    lw,
                    lh,
                    c,
                    c.x - min_x,
                    c.y - min_y,
                    color,
                    size,
                );
            }
        }
        if self.click_enabled {
            for c in &clicks {
                draw_click_ring(&mut layer, lw, lh, c, c.x - min_x, c.y - min_y);
            }
        }
        // 画光标
        if let Some((px, py)) = cursor_pos {
            let ox = px - self.cursor_hot.0 - min_x;
            let oy = py - self.cursor_hot.1 - min_y;
            blit_straight(
                &mut layer,
                lw,
                lh,
                &self.cursor_bits,
                self.cursor_w,
                self.cursor_h,
                ox,
                oy,
            );
        }
        Some(((min_x, min_y, lw, lh), layer))
    }
}

// ==================== GDI 光标位图捕获 ====================

/// 光标位图捕获：返回 (BGRA straight alpha top-down, w, h, 热点)
unsafe fn cursor_bitmap(
    hcur: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
) -> Option<(Vec<u8>, i32, i32, (i32, i32))> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    let mut ii = ICONINFO::default();
    if GetIconInfo(hcur, &mut ii).is_err() {
        return None;
    }
    // GetIconInfo 的位图句柄归调用者释放（RAII；hbmColor 为 NULL 时跳过）
    struct Bmp(windows::Win32::Graphics::Gdi::HBITMAP);
    impl Drop for Bmp {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(self.0);
            }
        }
    }
    let _mask = Bmp(ii.hbmMask);
    let color_valid = !ii.hbmColor.0.is_null();
    let _color = if color_valid { Some(Bmp(ii.hbmColor)) } else { None };
    let hc = ii.hbmColor;
    let hot = (ii.xHotspot as i32, ii.yHotspot as i32);

    let hdc = CreateCompatibleDC(None);
    if hdc.0.is_null() {
        return None;
    }
    struct Dc(windows::Win32::Graphics::Gdi::HDC);
    impl Drop for Dc {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
    let _dc = Dc(hdc);

    // 尺寸：彩色取 hbmColor；单色取 mask 半高（mask = AND + XOR 双层）
    let bmp_size = |hb: windows::Win32::Graphics::Gdi::HBITMAP| -> Option<(i32, i32)> {
        let mut bm = BITMAP::default();
        let r = GetObjectW(
            hb,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut core::ffi::c_void),
        );
        if r == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            None
        } else {
            Some((bm.bmWidth, bm.bmHeight))
        }
    };
    let (w, h) = if color_valid {
        bmp_size(hc)?
    } else {
        let (mw, mh) = bmp_size(ii.hbmMask)?;
        (mw, mh / 2)
    };
    if w > 256 || h > 256 || w <= 0 || h <= 0 {
        return None;
    }

    if color_valid {
        // 彩色光标：32bpp BGRA top-down 直读（alpha straight）
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: DIB_RGB_COLORS.0 as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let n = GetDIBits(
            hdc,
            hc,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bi,
            DIB_RGB_COLORS,
        );
        if n == 0 {
            return None;
        }
        return Some((buf, w, h, hot));
    }

    // 单色光标：1bpp mask（top-down 请求 2h 全高：前 h 行 AND，后 h 行 XOR）
    let row_bytes = ((w as usize) + 7) / 8;
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -(h * 2),
            biPlanes: 1,
            biBitCount: 1,
            biCompression: DIB_RGB_COLORS.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut mask = vec![0u8; row_bytes * h as usize * 2];
    let n = GetDIBits(
        hdc,
        ii.hbmMask,
        0,
        (h * 2) as u32,
        Some(mask.as_mut_ptr() as *mut core::ffi::c_void),
        &mut bi,
        DIB_RGB_COLORS,
    );
    if n == 0 {
        return None;
    }
    // 解码：AND=1 → 透明；AND=0 & XOR=0 → 黑；AND=0 & XOR=1 → 白
    //（AND=1 & XOR=1 = 反转光标，极罕见，按透明处理）
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let byte = x / 8;
            let bit = 7 - (x % 8) as u32;
            let and = (mask[y * row_bytes + byte] >> bit) & 1;
            if and != 0 {
                continue;
            }
            let xor = (mask[(h as usize + y) * row_bytes + byte] >> bit) & 1;
            let v = if xor == 1 { 255 } else { 0 };
            let d = (y * w as usize + x) * 4;
            buf[d] = v;
            buf[d + 1] = v;
            buf[d + 2] = v;
            buf[d + 3] = 255;
        }
    }
    Some((buf, w, h, hot))
}

// ==================== 图层绘制 ====================

/// straight alpha 位图顶合成（Windows 光标 alpha 语义为 straight）
fn blit_straight(
    dst: &mut [u8],
    dw: i32,
    dh: i32,
    src: &[u8],
    sw: i32,
    sh: i32,
    ox: i32,
    oy: i32,
) {
    for sy in 0..sh {
        let dy = oy + sy;
        if dy < 0 || dy >= dh {
            continue;
        }
        for sx in 0..sw {
            let dx = ox + sx;
            if dx < 0 || dx >= dw {
                continue;
            }
            let s = (sy * sw + sx) as usize * 4;
            let a = src[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = (dy * dw + dx) as usize * 4;
            if a == 255 {
                dst[d] = src[s];
                dst[d + 1] = src[s + 1];
                dst[d + 2] = src[s + 2];
                dst[d + 3] = 255;
            } else {
                let da = dst[d + 3] as u32;
                let out_a = a + da * (255 - a) / 255;
                if out_a == 0 {
                    continue;
                }
                for c in 0..3 {
                    let scc = src[s + c] as u32 * a;
                    let dcc = dst[d + c] as u32 * da * (255 - a) / 255;
                    dst[d + c] = ((scc + dcc) / out_a) as u8;
                }
                dst[d + 3] = out_a as u8;
            }
        }
    }
}

/// 点击圈当前半径（12 → 40 扩散）
fn click_radius(c: &ClickEvent) -> f64 {
    let t = c.at.elapsed().as_millis() as f64 / CLICK_DURATION_MS as f64;
    12.0 + 28.0 * t
}

/// 画点击扩散圈（圆环带 ~2.8px；左=青 / 右=红；alpha 随时间衰减）
fn draw_click_ring(layer: &mut [u8], lw: i32, lh: i32, c: &ClickEvent, cx: i32, cy: i32) {
    let t = c.at.elapsed().as_millis() as f64 / CLICK_DURATION_MS as f64;
    let alpha = (200.0 * (1.0 - t)) as u32;
    if alpha == 0 {
        return;
    }
    let r = click_radius(c);
    // 圈颜色 RGB（图层为 BGRA）：左=青 #37C6D9 / 右=红 #E5484D
    let (cr, cg, cb) = if c.button == 1 {
        (0xE5u32, 0x48, 0x4D)
    } else {
        (0x37, 0xC6, 0xD9)
    };
    let ri = r as i32;
    let band = 1.4f64;
    let y_lo = (cy - ri - 2).max(0);
    let y_hi = (cy + ri + 3).min(lh);
    let x_lo = (cx - ri - 2).max(0);
    let x_hi = (cx + ri + 3).min(lw);
    for y in y_lo..y_hi {
        for x in x_lo..x_hi {
            let dx = (x - cx) as f64;
            let dy = (y - cy) as f64;
            let d = (dx * dx + dy * dy).sqrt();
            if (d - r).abs() > band {
                continue;
            }
            let d4 = (y * lw + x) as usize * 4;
            let da = layer[d4 + 3] as u32;
            let out_a = alpha + da * (255 - alpha) / 255;
            if out_a == 0 {
                continue;
            }
            for (i, scc) in [cb, cg, cr].iter().enumerate() {
                let s = scc * alpha;
                let e = layer[d4 + i] as u32 * da * (255 - alpha) / 255;
                layer[d4 + i] = ((s + e) / out_a) as u8;
            }
            layer[d4 + 3] = out_a as u8;
        }
    }
}

/// 画高亮块（点击点为中心的半透明色块，1s 线性渐隐；左右键同色）。
/// 峰值不透明度 ~37%（94/255）——覆盖指示而不遮挡内容。
fn draw_highlight_block(
    layer: &mut [u8],
    lw: i32,
    lh: i32,
    c: &ClickEvent,
    cx: i32,
    cy: i32,
    color: [u8; 3],
    size: i32,
) {
    let t = c.at.elapsed().as_millis() as f64 / HIGHLIGHT_DURATION_MS as f64;
    if t >= 1.0 {
        return;
    }
    let alpha = (94.0 * (1.0 - t)) as u32;
    if alpha == 0 {
        return;
    }
    let half = (size / 2).max(4);
    let (cr, cg, cb) = (color[0] as u32, color[1] as u32, color[2] as u32);
    let y_lo = (cy - half).max(0);
    let y_hi = (cy + half).min(lh);
    let x_lo = (cx - half).max(0);
    let x_hi = (cx + half).min(lw);
    for y in y_lo..y_hi {
        for x in x_lo..x_hi {
            let d4 = (y * lw + x) as usize * 4;
            let da = layer[d4 + 3] as u32;
            let out_a = alpha + da * (255 - alpha) / 255;
            if out_a == 0 {
                continue;
            }
            for (i, scc) in [cb, cg, cr].iter().enumerate() {
                let s = scc * alpha;
                let e = layer[d4 + i] as u32 * da * (255 - alpha) / 255;
                layer[d4 + i] = ((s + e) / out_a) as u8;
            }
            layer[d4 + 3] = out_a as u8;
        }
    }
}

// ==================== 帧混合（SDR BGRA） ====================

/// SDR：图层 straight alpha 混入 BGRA 帧（帧 alpha 恒 255）。
/// pub(crate)：watermark.rs 复用同一混合路径（图层 = BGRA straight alpha + 帧内矩形）
pub(crate) fn blend_layer_bgra(
    frame: &mut [u8],
    stride: usize,
    w: i32,
    h: i32,
    layer: &[u8],
    lx: i32,
    ly: i32,
    lw: i32,
    lh: i32,
) {
    for y in 0..lh {
        let fy = ly + y;
        if fy < 0 || fy >= h {
            continue;
        }
        for x in 0..lw {
            let fx = lx + x;
            if fx < 0 || fx >= w {
                continue;
            }
            let s = (y * lw + x) as usize * 4;
            let a = layer[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = fy as usize * stride + fx as usize * 4;
            if a == 255 {
                frame[d] = layer[s];
                frame[d + 1] = layer[s + 1];
                frame[d + 2] = layer[s + 2];
            } else {
                let ia = 255 - a;
                frame[d] = ((layer[s] as u32 * a + frame[d] as u32 * ia) / 255) as u8;
                frame[d + 1] =
                    ((layer[s + 1] as u32 * a + frame[d + 1] as u32 * ia) / 255) as u8;
                frame[d + 2] =
                    ((layer[s + 2] as u32 * a + frame[d + 2] as u32 * ia) / 255) as u8;
            }
        }
    }
}

// ==================== 帧混合（HDR P010，PQ 域数学对偶 shader） ====================

/// PQ OETF（与 p010_gpu.rs shader 常数一致；输入 [0,1]=10000nits 归一线性）
fn pq_oetf(l: f64) -> f64 {
    if l <= 0.0 {
        return 0.0;
    }
    let l = l.min(1.0);
    const M1: f64 = 0.1593017578125;
    const M2: f64 = 78.84375;
    const C1: f64 = 0.8359375;
    const C2: f64 = 18.8515625;
    const C3: f64 = 18.6875;
    let lm = l.powf(M1);
    let np = (C1 + C2 * lm) / (1.0 + C3 * lm);
    np.powf(M2)
}

/// PQ EOTF（OETF 逆；返回 [0,1]=10000nits 归一线性）
fn pq_eotf(e: f64) -> f64 {
    const M1: f64 = 0.1593017578125;
    const M2: f64 = 78.84375;
    const C1: f64 = 0.8359375;
    const C2: f64 = 18.8515625;
    const C3: f64 = 18.6875;
    let e = e.clamp(0.0, 1.0);
    if e <= 0.0 {
        return 0.0;
    }
    let p = e.powf(1.0 / M2);
    let num = (p - C1).max(0.0);
    let den = C2 - C3 * p;
    if den <= 0.0 {
        return 1.0;
    }
    (num / den).powf(1.0 / M1)
}

fn srgb_eotf(e: f64) -> f64 {
    if e <= 0.04045 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb_oetf(l: f64) -> f64 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.0031308 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// HDR：图层混合入 P010 双平面。
/// 解码 10bit→YCbCr(PQ)→RGB(PQ)→EOTF→线性 BT.2020→BT.709→sRGB，sRGB 域混合
/// 后逆程（矩阵与 shader 同源）；UV 按 2×2 PQ 域 RGB 平均重下采样（对齐
/// shader box 语义）。pub(crate)：watermark.rs 复用
pub(crate) fn blend_layer_p010(
    y: &mut [u8],
    uv: &mut [u8],
    stride_y: usize,
    stride_uv: usize,
    w: i32,
    h: i32,
    layer: &[u8],
    lx: i32,
    ly: i32,
    lw: i32,
    lh: i32,
) {
    // 光标区域对齐到 2×2 网格（UV 重算边界）
    let x0 = (lx.max(0) & !1) as usize;
    let y0 = (ly.max(0) & !1) as usize;
    let x1 = (((lx + lw).min(w)) as usize + 1) & !1;
    let y1 = (((ly + lh).min(h)) as usize + 1) & !1;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let m_2020_709 = bt2020_to_bt709();
    let m_709_2020 = bt709_to_bt2020();
    let scale = 80.0 / 10000.0;

    // 2×2 块级处理（无覆盖块整体跳过——底层像素零漂移）：
    // 混合块内未覆盖像素也参与 UV 下采样（解码原值），保证色度连续
    for cy in (y0..y1).step_by(2) {
        'blk: for cx in (x0..x1).step_by(2) {
            // 块内 4 像素图层采样（先判覆盖）
            let mut cov = [(0u32, 0f64, 0f64, 0f64); 4]; // (a, r, g, b)
            let mut has_cover = false;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (px, py) = (cx + dx, cy + dy);
                    if px >= x1 || py >= y1 {
                        continue;
                    }
                    let sx = px as i32 - lx;
                    let sy = py as i32 - ly;
                    if sx < 0 || sy < 0 || sx >= lw || sy >= lh {
                        continue;
                    }
                    let s = (sy * lw + sx) as usize * 4;
                    let a = layer[s + 3] as u32;
                    if a > 0 {
                        has_cover = true;
                        cov[dy * 2 + dx] = (
                            a,
                            layer[s + 2] as f64 / 255.0,
                            layer[s + 1] as f64 / 255.0,
                            layer[s] as f64 / 255.0,
                        );
                    }
                }
            }
            if !has_cover {
                continue 'blk;
            }
            // 块内逐像素：解码 → 混合 → 重编码（覆盖像素）；未覆盖像素仅解码原值
            let mut sum = (0f64, 0f64, 0f64);
            let mut n = 0.0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (px, py) = (cx + dx, cy + dy);
                    if px >= x1 || py >= y1 {
                        continue;
                    }
                    let (yv, (uu, vv)) = (
                        read_p010_u16(y, stride_y, px, py),
                        read_p010_u16_pair(uv, stride_uv, px / 2, py / 2),
                    );
                    let (r, g, b) = p010_to_srgb(yv, uu, vv, &m_2020_709);
                    let (or_, og, ob, blended) = {
                        let (a, lr, lg, lb) = cov[dy * 2 + dx];
                        if a > 0 {
                            let af = a as f64 / 255.0;
                            (
                                lr * af + r * (1.0 - af),
                                lg * af + g * (1.0 - af),
                                lb * af + b * (1.0 - af),
                                true,
                            )
                        } else {
                            (r, g, b, false)
                        }
                    };
                    // PQ 域 RGB（覆盖像素重编码；未覆盖解码原值——UV 平均用）
                    let pq = {
                        let lin = (srgb_eotf(or_), srgb_eotf(og), srgb_eotf(ob));
                        let (l2r, l2g, l2b) =
                            m_709_2020.apply(lin.0 as f32, lin.1 as f32, lin.2 as f32);
                        (
                            pq_oetf(l2r as f64 * scale),
                            pq_oetf(l2g as f64 * scale),
                            pq_oetf(l2b as f64 * scale),
                        )
                    };
                    sum.0 += pq.0;
                    sum.1 += pq.1;
                    sum.2 += pq.2;
                    n += 1.0;
                    // Y 平面：仅覆盖像素写（未覆盖保持原值零漂移）
                    if blended {
                        let yy = 0.2627 * pq.0 + 0.6780 * pq.1 + 0.0593 * pq.2;
                        write_p010_u16(y, stride_y, px, py, quant_y(yy));
                    }
                }
            }
            // UV 下采样（块内 PQ 域平均 → BT.2100 YCbCr → 量化）
            if n == 0.0 {
                continue;
            }
            let (r, g, b) = (sum.0 / n, sum.1 / n, sum.2 / n);
            let yy = 0.2627 * r + 0.6780 * g + 0.0593 * b;
            let cb = (b - yy) / 1.8814;
            let cr = (r - yy) / 1.4746;
            write_p010_u16_pair(
                uv,
                stride_uv,
                cx / 2,
                cy / 2,
                quant_c(cb),
                quant_c(cr),
            );
        }
    }
}

/// P010 平面读：位置 (x,y) 的 16bit 样点（u16 LE；高 10 位有效）
fn read_p010_u16(plane: &[u8], stride: usize, x: usize, y: usize) -> u16 {
    let o = y * stride + x * 2;
    u16::from_le_bytes([plane[o], plane[o + 1]])
}

fn write_p010_u16(plane: &mut [u8], stride: usize, x: usize, y: usize, v: u16) {
    let o = y * stride + x * 2;
    plane[o] = (v & 0xFF) as u8;
    plane[o + 1] = (v >> 8) as u8;
}

/// UV 平面（R16G16）读/写：返回/写入 (U, V) u16 对
fn read_p010_u16_pair(plane: &[u8], stride: usize, x: usize, y: usize) -> (u16, u16) {
    let o = y * stride + x * 4;
    (
        u16::from_le_bytes([plane[o], plane[o + 1]]),
        u16::from_le_bytes([plane[o + 2], plane[o + 3]]),
    )
}

fn write_p010_u16_pair(plane: &mut [u8], stride: usize, x: usize, y: usize, u: u16, v: u16) {
    let o = y * stride + x * 4;
    plane[o] = (u & 0xFF) as u8;
    plane[o + 1] = (u >> 8) as u8;
    plane[o + 2] = (v & 0xFF) as u8;
    plane[o + 3] = (v >> 8) as u8;
}

/// P010 (Y10, U10, V10) → sRGB（BT.709）
fn p010_to_srgb(y10: u16, u10: u16, v10: u16, m_2020_709: &crate::color::matrix::Mat3) -> (f64, f64, f64) {
    let yn = (y10 as f64 - 64.0) / 876.0;
    let un = (u10 as f64 - 512.0) / 896.0;
    let vn = (v10 as f64 - 512.0) / 896.0;
    // PQ 域 RGB（NCL 逆矩阵——与 videorec renderer pq_to_linear 同源）
    let r = yn + 1.4746 * vn;
    let g = yn - 0.16455 * un - 0.57135 * vn;
    let b = yn + 1.8814 * un;
    // EOTF → 线性 BT.2020 → BT.709 → sRGB
    let lin = (pq_eotf(r), pq_eotf(g), pq_eotf(b));
    let (lr, lg, lb) = m_2020_709.apply(lin.0 as f32, lin.1 as f32, lin.2 as f32);
    (
        srgb_oetf(lr as f64),
        srgb_oetf(lg as f64),
        srgb_oetf(lb as f64),
    )
}

fn quant_y(y: f64) -> u16 {
    ((64.0 + 876.0 * y.clamp(-0.0693, 1.0693) + 0.5) as i64).clamp(64, 940) as u16
}

fn quant_c(c: f64) -> u16 {
    ((512.0 + 896.0 * c.clamp(-0.5, 0.5) + 0.5) as i64).clamp(64, 960) as u16
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_roundtrip() {
        for v in [0.001, 0.01, 0.1, 0.5, 0.9, 1.0] {
            let e = pq_oetf(v);
            let d = pq_eotf(e);
            assert!((d - v).abs() < 1e-9, "v={v} e={e} d={d}");
        }
        assert_eq!(pq_oetf(0.0), 0.0);
        assert_eq!(pq_eotf(0.0), 0.0);
        assert_eq!(pq_oetf(2.0), 1.0); // 超 1 钳制
    }

    #[test]
    fn srgb_roundtrip() {
        for v in [0.0, 0.02, 0.5, 0.9, 1.0] {
            let e = srgb_oetf(srgb_eotf(v));
            assert!((e - v).abs() < 1e-9, "v={v} e={e}");
        }
    }

    #[test]
    fn blend_bgra_opaque_cursor() {
        // 8x1 红色帧；2x1 白不透明图层 → 前两像素变白其余不变
        let mut frame = [0u8, 0, 255, 255].repeat(8);
        let layer = [255, 255, 255, 255, 255, 255, 255, 255];
        blend_layer_bgra(&mut frame, 8 * 4, 8, 1, &layer, 2, 0, 2, 1);
        assert_eq!(&frame[8..12], &[255, 255, 255, 255]); // x=2 白
        assert_eq!(&frame[12..16], &[255, 255, 255, 255]); // x=3 白
        assert_eq!(&frame[0..3], &[0, 0, 255]); // x=0 红不变
        assert_eq!(&frame[28..31], &[0, 0, 255]); // x=7 红不变
    }

    #[test]
    fn blend_bgra_half_alpha() {
        // 红底 + 半透明白 a=128 → (255*128 + 0*127)/255 = 128（整数除法）
        let mut frame = [0, 0, 255, 255];
        let layer = [255, 255, 255, 128];
        blend_layer_bgra(&mut frame, 4, 1, 1, &layer, 0, 0, 1, 1);
        assert_eq!(frame[0], 128);
        assert_eq!(frame[1], 128);
        assert_eq!(frame[2], 255);
    }

    #[test]
    fn blend_bgra_clip_out_of_frame() {
        // 图层完全在帧外 → 无变化不越界
        let mut frame = [10u8, 20, 30, 255];
        let layer = [255u8, 255, 255, 255];
        blend_layer_bgra(&mut frame, 4, 1, 1, &layer, 5, 5, 1, 1);
        assert_eq!(frame, [10, 20, 30, 255]);
    }

    #[test]
    fn blend_p010_white_cursor_on_black() {
        // 4x2 全黑 P010 + 1x1 白不透明图层 → 覆盖像素 Y 显著高于黑 (64)
        let (w, h) = (4usize, 2usize);
        let mut y = vec![0u8; w * h * 2];
        for i in 0..w * h {
            y[i * 2] = 64; // Y=64 (黑) LE
            y[i * 2 + 1] = 0;
        }
        let mut uv = vec![0u8; (w / 2) * (h / 2) * 4];
        for i in 0..(w / 2) * (h / 2) {
            uv[i * 4] = 0x00; // U=512 (0x200) LE
            uv[i * 4 + 1] = 0x02;
            uv[i * 4 + 2] = 0x00; // V=512
            uv[i * 4 + 3] = 0x02;
        }
        let layer = [255, 255, 255, 255];
        blend_layer_p010(&mut y, &mut uv, w * 2, 4, 4, 2, &layer, 1, 0, 1, 1);
        let y10_hit = read_p010_u16(&y, w * 2, 1, 0);
        assert!(y10_hit > 300, "白光标 Y={}（黑=64，白应 >300）", y10_hit);
        let y10_keep = read_p010_u16(&y, w * 2, 3, 1);
        assert_eq!(y10_keep, 64, "未覆盖像素应保持黑");
    }

    #[test]
    fn blend_p010_roundtrip_uncovered() {
        // a=0（全透明）时底层逐字不变
        let (w, h) = (2usize, 2usize);
        let mut y = vec![0u8; w * h * 2];
        for i in 0..w * h {
            let v = 500u16;
            y[i * 2] = (v & 0xFF) as u8;
            y[i * 2 + 1] = (v >> 8) as u8;
        }
        let mut uv = vec![0u8; (w / 2) * (h / 2) * 4];
        for i in 0..(w / 2) * (h / 2) {
            uv[i * 4 + 1] = 0x02; // U=V=512 (0x0200 LE)
            uv[i * 4 + 3] = 0x02;
        }
        let layer = [0u8; 2 * 2 * 4]; // 2x2 全透明图层
        blend_layer_p010(&mut y, &mut uv, w * 2, 4, 2, 2, &layer, 0, 0, 2, 2);
        assert_eq!(read_p010_u16(&y, w * 2, 0, 0), 500, "透明图层不得改变底层");
        assert_eq!(read_p010_u16(&y, w * 2, 1, 1), 500);
    }

    #[test]
    fn mono_cursor_decode_math() {
        // 单色光标解码逻辑对拍（独立于 GDI）：8 宽 1bpp，AND 全 0，XOR 交替
        let mask = [0b00000000u8, 0b10101010u8]; // [AND 行, XOR 行]
        let mut buf = vec![0u8; 8 * 4];
        for x in 0..8usize {
            let bit = 7 - (x % 8) as u32;
            let and = (mask[x / 8] >> bit) & 1;
            if and == 0 {
                let xor = (mask[1 + x / 8] >> bit) & 1;
                let v = if xor == 1 { 255 } else { 0 };
                let d = x * 4;
                buf[d] = v;
                buf[d + 1] = v;
                buf[d + 2] = v;
                buf[d + 3] = 255;
            }
        }
        assert_eq!(buf[0], 255); // x=0 XOR bit7=1 → 白
        assert_eq!(buf[3], 255);
        assert_eq!(buf[4], 0); // x=1 → 黑
        assert_eq!(buf[7], 255);
    }

    #[test]
    fn highlight_block_renders_and_fades() {
        // 20x20 透明图层 + 点击事件（刚发生）+ 黄色 16px 块 → 中心区域非零 alpha
        let mut layer = vec![0u8; 20 * 20 * 4];
        let c = ClickEvent {
            button: 0,
            x: 10,
            y: 10,
            at: Instant::now(),
        };
        draw_highlight_block(&mut layer, 20, 20, &c, 10, 10, [0xFF, 0xD4, 0x00], 16);
        // 中心像素：黄色混入（BGRA → B≈0 G≈D4 R≈FF）且 alpha > 0
        let d = (10 * 20 + 10) * 4;
        assert!(layer[d + 3] > 0, "中心 alpha 应非零");
        assert_eq!(layer[d + 2], 0xFF, "R 通道黄色");
        assert_eq!(layer[d + 1], 0xD4, "G 通道黄色");
        // 块外像素（half=8 → x=10±8=2..18；x=0 在块外）
        let out = (10 * 20 + 0) * 4;
        assert_eq!(layer[out + 3], 0, "块外像素不受影响");
        // 完全过期（>1s）→ 不绘制
        let old = ClickEvent {
            button: 0,
            x: 10,
            y: 10,
            at: Instant::now() - std::time::Duration::from_millis(1100),
        };
        let mut layer2 = vec![0u8; 20 * 20 * 4];
        draw_highlight_block(&mut layer2, 20, 20, &old, 10, 10, [0xFF, 0xD4, 0x00], 16);
        assert_eq!(layer2[(10 * 20 + 10) * 4 + 3], 0, "过期事件不绘制");
    }
}
