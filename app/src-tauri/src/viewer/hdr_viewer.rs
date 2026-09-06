//! 真 HDR 查看窗口（原生 Win32 + D3D11 scRGB FP16 交换链）
//!
//! WebView2/Chromium 渲染管线不支持 HDR 输出，所有网页预览都必然走
//! 「色调映射 → SDR」。真 HDR 显示（1000 nits 高光在 HDR 屏上真实点亮
//! 1000 nits）必须绕过 WebView2：本模块用纯 Win32 窗口 + D3D11 翻转
//! 交换链，格式 R16G16B16A16_FLOAT、色彩空间
//! DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709（线性 scRGB，1.0 = 80 nits）。
//!
//! 数据通路零转换：解码层的 HdrSource 本就是 scRGB f16（BT.709 线性光，
//! 绝对亮度语义 1.0 = 80 nits），直接上传纹理；DWM 高级合成按物理 nits
//! 点亮 —— 与截图时的原始屏幕逐像素一致（屏幕白 200 nits → scRGB 2.5 →
//! 仍显示 200 nits；HDR 高光 1000 nits → 12.5 → 仍显示 1000 nits）。
//!
//! 交互：滚轮缩放（以光标为锚）、左键拖拽平移、双击 1:1/自适应、Esc 关闭。
//! 窗口移动到非 HDR 显示器时标题栏提示（DWM 会按 SDR 裁剪 >1.0 部分）。

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

use serde::Serialize;
use windows::core::{w, Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, ID3DInclude, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11InputLayout,
    ID3D11PixelShader, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_VERTEX_BUFFER, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_INPUT_ELEMENT_DESC,
    D3D11_INPUT_PER_VERTEX_DATA, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD,
    D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory2, IDXGIOutput,
    IDXGISwapChain, IDXGISwapChain1, IDXGISwapChain3, DXGI_ADAPTER_DESC1, DXGI_PRESENT,
    DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, MonitorFromWindow,
    ScreenToClient, UpdateWindow, ValidateRect, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, KillTimer, LoadCursorW, LoadIconW,
    PostMessageW,
    PostQuitMessage, RegisterClassExW, SetCursor, SetTimer, SetWindowLongPtrW, SetWindowPos,
    SetWindowTextW, ShowWindow, TranslateMessage, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW,
    EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, GWLP_USERDATA, HWND_TOP,
    IDC_ARROW, IDC_SIZEALL, IDI_APPLICATION, MINMAXINFO, MSG, OBJID_WINDOW, SIZE_MINIMIZED,
    SWP_NOACTIVATE, SWP_NOZORDER, SWP_FRAMECHANGED, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_CREATE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND,
    WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_MOVE, WM_NCCREATE, WM_PAINT, WM_SIZE, WM_TIMER, WNDCLASSEXW, WNDCLASS_STYLES,
    GWL_EXSTYLE, GWL_STYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP,
    WS_VISIBLE,
};

use tauri::{Emitter, Manager, WebviewWindowBuilder};

use crate::capture::monitor::{enumerate_monitors, get_sdr_white_level_nits, MonitorInfo};
use crate::upscale::d3d11;
use crate::viewer::anim_pool::{AnimFramePool, GpuFrame, PoolState, PoolTex};
use crate::viewer::decode::{ensure_hdr_source, HdrSource, ImageKind};

// ==================== 对外命令 ====================

/// HDR 显示器状态（前端用于决定「真 HDR 查看」按钮显隐 + 徽标展示）
#[derive(Debug, Clone, Serialize)]
pub struct HdrDisplayInfo {
    /// 是否存在任一已开启 HDR 的显示器
    pub any_hdr: bool,
    /// HDR 显示器最大全帧亮度（nits，取所有 HDR 显示器最大值）
    pub max_luminance: f32,
    /// 系统 SDR 白电平（nits）
    pub sdr_white_nits: f32,
    /// 面板原生色域分类（BT.709 / DCI-P3 / BT.2020；未识别为 null）
    pub gamut_name: Option<String>,
    /// 面板对 DCI-P3 的覆盖率（0-1）
    pub p3_coverage: Option<f32>,
    /// 面板对 BT.2020 的覆盖率（0-1）
    pub bt2020_coverage: Option<f32>,
}

#[tauri::command]
pub fn hdr_display_info() -> HdrDisplayInfo {
    let monitors = enumerate_monitors().unwrap_or_default();
    let hdrs: Vec<&MonitorInfo> = monitors.iter().filter(|m| m.is_hdr()).collect();
    // 面板原生色域（AdvancedColorInfo 实测；DXGI 峰值报 0 时优先用它）
    let gamut = crate::capture::monitor::get_panel_gamut();
    // 峰值 = 小面积 MaxLuminance（HDR 徽标语义，与其它软件一致）；
    // 驱动偶发报 0 时回退全帧值
    let peak = hdrs.iter().map(|m| m.max_luminance).fold(0.0_f32, f32::max);
    let max_luminance = if peak > 0.0 {
        peak
    } else {
        hdrs.iter()
            .map(|m| m.max_full_frame_luminance)
            .fold(0.0_f32, f32::max)
    }
    .max(gamut.as_ref().map(|g| g.max_luminance).unwrap_or(0.0));
    HdrDisplayInfo {
        any_hdr: !hdrs.is_empty(),
        max_luminance,
        sdr_white_nits: get_sdr_white_level_nits(),
        gamut_name: gamut.as_ref().map(|g| g.name.clone()),
        p3_coverage: gamut.as_ref().map(|g| g.p3_coverage),
        bt2020_coverage: gamut.as_ref().map(|g| g.bt2020_coverage),
    }
}

/// 打开真 HDR 查看（原生 D3D11 scRGB 交换链，绕过 WebView2）
///
/// 嵌入模式（默认）：viewer 窗口存在时创建 WS_CHILD 子窗口，作为看图界面的
/// 图片容器——WebView2 的工具栏/状态栏/缩略图条全部保留，前端把容器区域
/// 矩形同步过来（set_hdr_view_rect），子窗口精确覆盖该区域。
/// 独立模式（兜底）：无 viewer 窗口时回退为独立顶层窗口。
#[tauri::command]
pub async fn open_hdr_viewer(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    // ITM 合成键（itm:<源路径>）：is_file/格式门检查真实源文件；HDR 源走 ITM 缓存
    let itm_key = p.to_string_lossy().starts_with("itm:");
    let real = if itm_key {
        let r = crate::viewer::decode::itm_ai::parse_itm_key(&p)
            .ok_or_else(|| format!("ITM 键非法: {path}"))?;
        if !r.is_file() {
            log::warn!("[真HDR] 打开失败：ITM 源文件不存在 {}", r.display());
            return Err(format!("源文件不存在: {}", r.display()));
        }
        Some(r)
    } else if !p.is_file() {
        log::warn!("[真HDR] 打开失败：文件不存在 {}", path);
        return Err(format!("文件不存在: {}", path));
    } else {
        None
    };
    // 仅 HDR 线性源可直通 scRGB 纹理（ITM 键跳过——产物已是 scRGB f16）
    if !itm_key {
        match crate::viewer::decode::classify_with_content(&p) {
            ImageKind::Exr | ImageKind::PngHdr | ImageKind::Jxl | ImageKind::Jxr => {}
            other => {
                log::warn!(
                    "[真HDR] 打开失败：格式不含 HDR 线性数据 {:?} {}",
                    other,
                    path
                );
                return Err("该格式不含 HDR 线性数据（支持 EXR / HDR PNG / JXL / JXR）".to_string());
            }
        }
    }
    // HDR 源：优先 LRU 缓存，未命中则完整解码一次（解码内部会缓存）
    let src = if itm_key {
        crate::viewer::decode::itm_ai::ensure_itm_source(&p).ok_or_else(|| {
            log::error!("[真HDR] 打开失败：ITM 源不可用（重跑失败）: {path}");
            "ITM HDR 源不可用（重跑失败，查看日志）".to_string()
        })?
    } else {
        match ensure_hdr_source(&p) {
            Some(s) => s,
            None => {
                log::error!(
                    "[真HDR] 打开失败：HDR 源解码失败（文件可能为 SDR 变体/被占用/LRU 竞态）: {}",
                    path
                );
                return Err("HDR 源解码失败（文件可能为 SDR 变体，无 HDR 数据）".to_string());
            }
        }
    };
    let _ = real; // 日志与错误提示用（真实源路径已在上文校验）

    // 关闭旧嵌入视图（切图/重开场景；旧线程异步销毁，全局句柄在此原子取出）
    close_embedded();

    // 嵌入父窗口：viewer 的 HWND（子窗口随主窗口移动/最小化，无需自管理）
    let viewer_hwnd = app
        .get_webview_window("viewer")
        .and_then(|w| w.hwnd().ok())
        .map(|h| SendHwnd(HWND(h.0 as *mut core::ffi::c_void)));
    let anchor = viewer_hwnd.map(|s| s.0);
    let monitor = pick_monitor(anchor);
    if !monitor.is_hdr() {
        return Err(format!(
            "显示器（{}）未开启 Windows HDR，无法真 HDR 显示；请先在系统设置 → 屏幕 → HDR 中开启",
            monitor.device_name
        ));
    }

    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (w, h) = (src.width, src.height);
    let startup = Startup {
        src,
        name,
        monitors: enumerate_monitors().unwrap_or_default(),
        app: app.clone(),
        embedded: viewer_hwnd.is_some(),
        anim: None,
    };
    // 独立模式初始窗口：显示器 82% 居中（嵌入模式矩形由前端 set_hdr_view_rect 下发）
    let m = monitor.rect();
    let mw = m.right - m.left;
    let mh = m.bottom - m.top;
    let win_w = ((mw as f32 * 0.82) as i32).max(320).min(mw);
    let win_h = ((mh as f32 * 0.82) as i32).max(240).min(mh);
    let rect = RECT {
        left: m.left + (mw - win_w) / 2,
        top: m.top + (mh - win_h) / 2,
        right: m.left + (mw - win_w) / 2 + win_w,
        bottom: m.top + (mh - win_h) / 2 + win_h,
    };
    std::thread::Builder::new()
        .name("jietu-hdr-view".into())
        .spawn(move || run_window(startup, rect, viewer_hwnd.map(|s| s.0)))
        .map_err(|e| format!("创建 HDR 查看线程失败: {}", e))?;
    log::info!(
        "[真HDR] 窗口已启动: {} ({}x{}, {} 峰值 {}nits)",
        path,
        w,
        h,
        if viewer_hwnd.is_some() {
            "嵌入模式"
        } else {
            "独立模式"
        },
        monitor.max_full_frame_luminance
    );
    Ok(())
}

// ==================== 动画回放（P3；JXL 动图流式播放） ====================

/// 探测 JXL 是否为动图（前端决定显示「播放动图」按钮）
///
/// async 命令在 tokio 线程池执行；读盘 + BasicInfo 探测移入 spawn_blocking，
/// 避免录制产物（数百 MB）读文件冻结主线程（同步命令跑在主线程）
#[tauri::command]
pub async fn probe_animation(path: String) -> Result<bool, String> {
    tokio::task::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !p.is_file() {
            return Ok(false);
        }
        super::decode::jxl::probe_is_animation_head(&p)
    })
    .await
    .map_err(|e| format!("探测任务失败: {}", e))?
}

/// 打开动画播放窗口（独立模式；WM_TIMER 驱动逐帧翻页，无限循环）
///
/// 两条播放路径（回退树，config.toml `[viewer]`）：
/// - **GPU 常驻显存池**（`anim_playback="gpu"`，默认）：帧以 RGBA16F 池纹理
///   常驻显存，填充线程直写（hybrid：GPU 链零 CPU 像素；native：CPU 解码 +
///   引擎锁内 UpdateSubresource 上传），窗口直接绑定池纹理渲染 + 引擎设备
///   交换链。生效条件：GPU 引擎硬件可用（非 WARP）&& 显存预算 ≥ 3 帧 &&
///   池创建/首帧入池成功——任一不满足即回退。
/// - **内存环形缓冲**（AnimRing）：解码线程 CPU 解帧推入环形（4GB 封顶背压），
///   窗口线程按帧 duration 定时 UpdateSubresource 上传（原路径，代码保留）。
#[tauri::command]
pub async fn open_animation_player(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("文件不存在: {}", path));
    }

    // 0. 播放路径决策（回退树；native/hybrid 后端均走 GPU 池——native 经
    //    UpdateSubresource 入池，hybrid 经 GPU 链直写入池，见 try_open_gpu_pool）
    let cfg = crate::config::Config::load();
    let backend = super::decode::jxl::AnimBackend::parse(&cfg.viewer.anim_backend);
    let want_gpu = parse_anim_playback(&cfg.viewer.anim_playback);

    let mut gpu: Option<AnimGpuStartup> = None;
    if want_gpu {
        match try_open_gpu_pool(&p, cfg.viewer.anim_vram_mb, backend) {
            Ok(g) => gpu = Some(g),
            Err(e) => log::warn!("[动画] GPU 常驻显存播放不可用，回退 ring 路径: {}", e),
        }
    }

    if let Some(g) = gpu {
        return spawn_anim_pool_window(app, p, g);
    }
    open_animation_player_ring(app, p).await
}

/// `[viewer] anim_playback` 解析（容错：非法值/空串 → "gpu" 默认；"ring" → ring）
fn parse_anim_playback(s: &str) -> bool {
    !matches!(s.trim().to_ascii_lowercase().as_str(), "ring")
}

/// GPU 池播放路径启动包（open 命令线程 → 窗口/填充线程）
struct AnimGpuStartup {
    pool: std::sync::Arc<AnimFramePool>,
    dec: super::decode::jxl::AnimPlayerDecoder,
    first_duration_ms: u32,
    backend: super::decode::jxl::AnimBackend,
}

/// 尝试进入 GPU 常驻显存池模式（回退树逐级判定 + 池创建 + 首帧入池）
///
/// 1. engine() 硬件可用（WARP 软渲染不进池——compute 重建无收益还占预算）
/// 2. 显存预算：`min(配置 MB, 专用显存 50% − 512MB)`（0=auto 时取
///    `min(4096, 同式)`）；capacity = 预算/帧字节，< 3 帧不进池
/// 3. 池预分配成功 + 首帧入池（同步，窗口纹理初始化即用；native = CPU 解码
///    UpdateSubresource，hybrid = 快照 GPU 链直写）
fn try_open_gpu_pool(
    p: &PathBuf,
    vram_mb_cfg: u32,
    backend: super::decode::jxl::AnimBackend,
) -> Result<AnimGpuStartup, String> {
    // 1. 引擎检查（先于解码器打开：条件不满足不浪费 libjxl open）
    let eng = d3d11::engine()
        .as_ref()
        .map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let (device, is_warp) = {
        let g = eng.0.lock().map_err(|e| format!("GPU 引擎锁: {e}"))?;
        let (d, _c) = g.device_ctx();
        (d.clone(), g.is_warp)
    };
    if is_warp {
        return Err("GPU 引擎为 WARP 软渲染".into());
    }

    // 2. 打开播放解码器（native/hybrid 按配置）+ 尺寸 → 帧字节
    let mut dec = super::decode::jxl::AnimPlayerDecoder::open(p, backend)?;
    let (w, h) = dec.dims();
    if w == 0 || h == 0 {
        return Err("动画尺寸异常".into());
    }
    let frame_bytes = w as u64 * h as u64 * 8; // RGBA16F

    // 3. 显存预算 → 容量（专用显存取自 engine 实际所用 adapter）
    let dedicated = unsafe { dedicated_vram_bytes(&device) };
    let budget_mb = anim_pool_budget_mb(vram_mb_cfg, dedicated);
    let capacity = (budget_mb * 1024 * 1024 / frame_bytes) as usize;
    if capacity < ANIM_POOL_MIN_CAPACITY {
        return Err(format!(
            "显存预算 {}MB 仅容 {} 帧（每帧 {}MB），需 ≥ {} 帧",
            budget_mb,
            capacity,
            frame_bytes / 1024 / 1024,
            ANIM_POOL_MIN_CAPACITY
        ));
    }
    log::info!(
        "[动画] GPU 池: {}x{} 每帧 {:.1}MB × {} 槽 = {:.0}MB（预算 {}MB，专用显存 {}MB）",
        w,
        h,
        frame_bytes as f64 / 1024.0 / 1024.0,
        capacity,
        capacity as f64 * frame_bytes as f64 / 1024.0 / 1024.0,
        budget_mb,
        dedicated.map(|d| d / 1024 / 1024).unwrap_or(0)
    );

    // 4. 池创建（预分配失败 → Err → 回退 ring；已建纹理随 Drop 归还显存）
    let pool = AnimFramePool::new((w, h), capacity, crate::viewer::anim_pool::d3d_factory(device, (w, h)))
        .map_err(|e| format!("纹理池创建失败: {e}"))?;

    // 5. 首帧同步入池（idx=0）：native = CPU 解帧 → 引擎锁内 UpdateSubresource
    //    直传槽纹理；hybrid = 快照 → 引擎锁内 GPU 链直写槽纹理
    let slot = pool
        .acquire_free()
        .ok_or_else(|| "池预分配后无空闲槽".to_string())?;
    let tex = pool
        .slot_tex(slot)
        .ok_or_else(|| "池槽纹理缺失".to_string())?;
    let dur = match backend {
        super::decode::jxl::AnimBackend::Native => {
            let frame = dec
                .next_frame()?
                .ok_or_else(|| "动画无有效帧".to_string())?;
            native_upload_f16_into(&tex.tex, &frame)
                .map_err(|e| format!("首帧入池失败: {e}"))?;
            frame.duration_ms
        }
        super::decode::jxl::AnimBackend::Hybrid => {
            let gamut = dec.gamut();
            let (snap, dur) = dec
                .next_frame_snapshot()?
                .ok_or_else(|| "动画无有效帧".to_string())?;
            let filled =
                crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16_into(&snap, &gamut, &tex.tex);
            drop(snap);
            filled.map_err(|e| format!("首帧 GPU 直写失败: {e}"))?;
            dur
        }
    };
    pool.publish(slot, 0, dur);
    log::info!(
        "[动画] 首帧已入池（idx=0，{}ms，后端 {}），切换 GPU 常驻显存播放",
        dur,
        anim_backend_name(backend)
    );

    Ok(AnimGpuStartup {
        pool,
        dec,
        first_duration_ms: dur,
        backend,
    })
}

/// 专用显存字节数（engine 实际所用 adapter；等价于设计稿的
/// IDXGIFactory1::EnumAdapters 首卡查询，但精确指向 engine device 所在卡）
unsafe fn dedicated_vram_bytes(device: &ID3D11Device) -> Option<u64> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    let adapter: IDXGIAdapter = dxgi.GetAdapter().ok()?;
    let a1: IDXGIAdapter1 = adapter.cast().ok()?;
    let desc: DXGI_ADAPTER_DESC1 = a1.GetDesc1().ok()?;
    Some(desc.DedicatedVideoMemory as u64)
}

/// GPU 池显存预算（MB）：`min(配置, 专用显存 50% − 512MB)`；
/// 配置 0 = auto = `min(4096, 同式)`；专用显存查询失败按 auto 上限兜底
const ANIM_VRAM_AUTO_CAP_MB: u64 = 4096;
const ANIM_VRAM_HEADROOM_MB: u64 = 512;
const ANIM_POOL_MIN_CAPACITY: usize = 3;

fn anim_pool_budget_mb(cfg_mb: u32, dedicated_bytes: Option<u64>) -> u64 {
    let mb = 1024u64 * 1024;
    let cap = dedicated_bytes
        .map(|d| (d / 2).saturating_sub(ANIM_VRAM_HEADROOM_MB * mb) / mb)
        .unwrap_or(ANIM_VRAM_AUTO_CAP_MB);
    if cfg_mb == 0 {
        ANIM_VRAM_AUTO_CAP_MB.min(cap)
    } else {
        (cfg_mb as u64).min(cap)
    }
}

/// 后端名（AnimGpuStartup/AnimStartup/anim://fill 的 backend 字段）
fn anim_backend_name(b: super::decode::jxl::AnimBackend) -> &'static str {
    match b {
        super::decode::jxl::AnimBackend::Native => "native",
        super::decode::jxl::AnimBackend::Hybrid => "hybrid",
    }
}

/// native 后端单帧入池：引擎锁内 `UpdateSubresource` 全纹理上传
///
/// f16 scRGB RGBA 行交错（pitch = w×8）直传 RGBA16F 槽纹理——UpdateSubresource
/// 无格式转换，纹理内容与 `AnimFrame.data` 逐位一致。pDstBox=None（全纹理）；
/// 调用前该纹理不得绑在 context 上（draw_frame 尾部已解绑 PS SRV；池纹理
/// 仅在渲染帧内以 SRV 绑定）。单次调用完成，不拆分引擎锁。
pub(super) fn native_upload_f16_into(
    tex: &ID3D11Texture2D,
    frame: &super::decode::jxl::AnimFrame,
) -> Result<(), String> {
    let eng = d3d11::engine()
        .as_ref()
        .map_err(|e| format!("GPU 引擎不可用: {e}"))?;
    let g = eng.0.lock().map_err(|e| format!("GPU 引擎锁: {e}"))?;
    let (_device, ctx) = g.device_ctx();
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        let row_pitch = desc.Width * 8; // RGBA16F = 8 B/px
        let depth_pitch = row_pitch * desc.Height;
        if frame.data.len() as u64 != depth_pitch as u64 {
            return Err(format!(
                "帧字节 {} 与槽纹理 {}x{} RGBA16F（{}B）不匹配",
                frame.data.len(),
                desc.Width,
                desc.Height,
                depth_pitch
            ));
        }
        ctx.UpdateSubresource(
            tex,
            0,
            None,
            frame.data.as_ptr() as *const core::ffi::c_void,
            row_pitch,
            depth_pitch,
        );
    }
    Ok(())
}

/// GPU 池填充线程入口：独立帧产物（native）→ 并行填充探测；否则单线程。
/// （详细语义见 fill_gpu_pool_single / fill_gpu_pool_parallel）
fn fill_gpu_pool(
    pool: std::sync::Arc<AnimFramePool>,
    dec: super::decode::jxl::AnimPlayerDecoder,
    path: PathBuf,
    app: tauri::AppHandle,
    backend: super::decode::jxl::AnimBackend,
) {
    let backend_name = anim_backend_name(backend);
    // 独立帧并行填充（native only）：新录制产物每帧独立可 skip——N 个解码器
    // 相位并行解码。worker 数按物理核（available_parallelism 返回逻辑处理器数，
    // AMD 7745HX = 8C16T → 物理核 = 逻辑/2）：4 worker × 4 线程 runner =
    // 16 逻辑槽位占满；worker 数上限 4（更多则帧内组级并行过度摊薄）。
    // 同时设 JXL_PLAY_WORKERS 环境变量（进程级）：AnimationDecoder::open 按它
    // 缩减每个解码器的 runner 线程数，避免 4×15 线程超订。
    if backend == super::decode::jxl::AnimBackend::Native {
        let physical_cores = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).max(1))
            .unwrap_or(1);
        let workers = physical_cores.clamp(1, 4);
        if workers > 1 {
            std::env::set_var("JXL_PLAY_WORKERS", workers.to_string());
            if fill_gpu_pool_parallel(&pool, &path, &app, backend, backend_name, workers, 0) {
                return; // 并行填充完成（含 EOF/全驻留处理）
            }
            std::env::remove_var("JXL_PLAY_WORKERS"); // 回退路径：恢复默认 runner 配置
        }
    }
    fill_gpu_pool_single(pool, dec, path, app, backend, backend_name)
}

/// 并行填充：worker 池（各持解码器相位 skip 定位）+ 本线程有序发布。
/// 返回 true = 已接手填充（探测成功）；false = 探测失败回退单线程。
///
/// `start_frame`：会话基帧（已同步入池为 seq 0，worker 从 start_frame+1 起
/// 填；首次打开 = 0，seek 重启 = target）。
fn fill_gpu_pool_parallel(
    pool: &std::sync::Arc<AnimFramePool>,
    path: &std::path::Path,
    app: &tauri::AppHandle,
    backend: super::decode::jxl::AnimBackend,
    backend_name: &str,
    workers: usize,
    start_frame: u64,
) -> bool {
    // 探测：skip(1) 后能否解出帧？旧产物（帧间引用）skip 后 Err/None → 回退。
    // seek 重启路径（start_frame>0）跳过探测——首次打开已确认独立帧产物
    if start_frame == 0 {
        let mut probe =
            match super::decode::jxl::AnimPlayerDecoder::open_skip_native(path, backend, 1) {
                Ok(d) => d,
                Err(_) => return false,
            };
        match probe.next_frame_pq16() {
            Ok(Some(_)) => {} // 独立帧确认
            _ => {
                log::warn!("[动画] skip 探测无帧（旧产物帧间引用），回退单线程填充");
                return false;
            }
        }
        drop(probe);
    }
    log::info!(
        "[动画] 独立帧区间分工并行填充：{} worker × 批 16 帧（native，会话基帧 {}）",
        workers,
        start_frame
    );

    // 区间领批（轮次循环）：每 worker 滚动领取 [start, start+BATCH) 区间，
    // open_at(start)（consume_header 停在 FRAME 前 → skip 精确跳到帧 start，
    // 无快照成本）→ 顺序解整批。对比相位过滤（每 worker 解全文件只发 1/N，
    // 有效吞吐 = 1 个缩减线程数解码器 = 11fps 实测）：区间分工每帧只解一次，
    // 4 worker × 4 线程实测 59fps。
    // 滑窗长片（total > 槽位）：每轮 EOF 后回卷游标续填下一轮（对齐单线程
    // 版语义），seq 全局单调（第二轮文件帧 0 → seq total）。
    const BATCH: u64 = 16;
    let cursor = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    let eof_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 有序发布器（本线程）状态：严格按序发布——仅当重排缓冲最小键 ==
    // next_idx（全局发布序号）才弹出；无条件 pop_first 会把慢批帧重编号
    // → 播放端"来回跳动"bug
    let mut next_idx: u64 = 1; // 全局发布序号（0 = 首帧，打开流程已入池）
    let mut filled_since_emit: u32 = 0;
    let mut total_frames: u64 = 0; // 首轮 EOF 后已知
    let mut round_offset: u64 = 0; // seq 偏移：首轮 0；第 n 轮 = (n-1)*total
    loop {
        // 游标回卷：首轮从 start_frame+1 起（基帧已同步入池，不重发）；
        // 后续轮从 0 起（第二轮起帧 0 也由 worker 填，→ seq total+n）
        cursor.store(
            if round_offset == 0 { start_frame + 1 } else { 0 },
            std::sync::atomic::Ordering::SeqCst,
        );
        eof_seen.store(false, std::sync::atomic::Ordering::Release);
        let round_start_seq = next_idx;
        // 有界 channel（容量 = 一个批）：池满背压链——发布器 acquire_free_wait
        // 等播放端释放槽 → 不消费 rx → channel 满 → worker send 阻塞 → 解码
        // 暂停。若无界：60fps 文件播放 15s，积压 ~690 帧 × 16.6MB ≈ 11.5GB
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, AnimFillInput)>(BATCH as usize);
        let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::with_capacity(workers);
        for _ in 0..workers {
            let tx = tx.clone();
            let path = path.to_path_buf();
            let cursor = std::sync::Arc::clone(&cursor);
            let eof_seen = std::sync::Arc::clone(&eof_seen);
            handles.push(
                std::thread::Builder::new()
                    .name("anim-fill-w".into())
                    .spawn(move || {
                        loop {
                            if eof_seen.load(std::sync::atomic::Ordering::Acquire) {
                                return; // 别的 worker 已确认 EOF
                            }
                            let start =
                                cursor.fetch_add(BATCH, std::sync::atomic::Ordering::SeqCst);
                            // open_at：consume_header 停在 FRAME 事件前 +
                            // skip(start) → 首帧 = start（落点模型已 7 点验证）
                            let mut dec = match super::decode::jxl::AnimationDecoder::open_at(
                                &path, start,
                            ) {
                                Ok(d) => d,
                                Err(_) => {
                                    eof_seen.store(true, std::sync::atomic::Ordering::Release);
                                    return;
                                }
                            };
                            let mut got_any = false;
                            let mut idx = start;
                            while idx < start + BATCH {
                                match dec.next_frame_pq16() {
                                    Ok(Some((frame, dur))) => {
                                        got_any = true;
                                        if tx
                                            .send((idx, AnimFillInput::Pq16(frame, dur)))
                                            .is_err()
                                        {
                                            return; // 发布器退出（池关闭）
                                        }
                                        idx += 1;
                                    }
                                    _ => break, // EOF / 解码错误：批提前结束
                                }
                            }
                            if !got_any {
                                // 整批无帧 = start 越过文件尾（skip 超尾 → 全跳）
                                eof_seen.store(true, std::sync::atomic::Ordering::Release);
                                return;
                            }
                        }
                    })
                    .expect("spawn anim-fill-w"),
            );
        }
        drop(tx);
        if handles.is_empty() {
            log::warn!("[动画] 并行 worker 全部启动失败，回退单线程");
            return false;
        }

        // 发布器：严格按序弹出。seq 映射：首轮 = file_idx - start_frame
        //（会话基帧对齐 seq 0；首次打开 start_frame=0 即 file_idx）；后续轮
        //= round_offset + file_idx（第二轮文件帧 0 → seq 会话总数）
        let mut reorder: std::collections::BTreeMap<u64, AnimFillInput> =
            std::collections::BTreeMap::new();
        for (file_idx, input) in rx {
            let seq = if round_offset == 0 {
                file_idx - start_frame
            } else {
                round_offset + file_idx
            };
            reorder.insert(seq, input);
            while reorder.keys().next() == Some(&next_idx) {
                let (_, inp) = reorder.pop_first().unwrap();
                let Some(slot) = pool.acquire_free_wait() else {
                    return true; // 池关闭
                };
                let dur = match &inp {
                    AnimFillInput::Pq16(_, d) => *d,
                    AnimFillInput::Snapshot(_, d) => *d,
                };
                let write = match pool.slot_tex(slot) {
                    Some(t) => match &inp {
                        AnimFillInput::Pq16(frame, _) => {
                            crate::viewer::decode::pq16_gpu::convert_into(frame, &t.tex)
                        }
                        AnimFillInput::Snapshot(..) => Err("并行填充无快照分支".into()),
                    },
                    None => Err(format!("池槽 {} 纹理缺失", slot)),
                };
                drop(inp);
                match write {
                    Ok(()) => {
                        pool.publish(slot, next_idx, dur);
                        next_idx += 1;
                        filled_since_emit += 1;
                        if filled_since_emit >= 30 {
                            filled_since_emit = 0;
                            emit_anim_fill(app, pool, backend_name);
                        }
                    }
                    Err(e) => {
                        log::warn!("[动画] GPU 池填充失败（帧 {}）: {}", next_idx, e);
                        pool.set_degraded();
                        emit_anim_fill(app, pool, backend_name);
                        return true;
                    }
                }
            }
        }
        // 通道关闭 = 本轮全体 worker 退出（EOF）
        for h in handles.drain(..) {
            let _ = h.join();
        }
        if total_frames == 0 {
            // 首轮 EOF：确定总帧数（帧 0 同步入池 + seq 1..next_idx-1）
            total_frames = next_idx;
            if total_frames == 0 {
                log::warn!("[动画] 并行填充零帧，回退单线程");
                return false;
            }
            pool.set_total(total_frames);
            log::info!(
                "[动画] 全片完成池化：{} 帧（区间分工并行填充）",
                total_frames
            );
            emit_anim_fill(app, pool, backend_name);
            if total_frames <= pool.capacity() as u64 {
                log::info!(
                    "[动画] 全驻留：{} 帧 ≤ {} 槽，填充挂起（循环播放零解码）",
                    total_frames,
                    pool.capacity()
                );
                pool.wait_closed();
                return true;
            }
        } else if next_idx == round_start_seq {
            // 续填轮零帧（文件异常，首轮有帧）→ 挂起防无限空转
            log::warn!("[动画] 滑窗续填轮零帧（文件异常），填充挂起");
            return true;
        }
        round_offset += total_frames; // 下一轮
    }
}


/// 单线程顺序填充（原填充逻辑）
fn fill_gpu_pool_single(
    pool: std::sync::Arc<AnimFramePool>,
    mut dec: super::decode::jxl::AnimPlayerDecoder,
    path: PathBuf,
    app: tauri::AppHandle,
    backend: super::decode::jxl::AnimBackend,
    backend_name: &str,
) {
    let mut frame_idx: u64 = 1; // 0 已在打开流程同步入池
    let mut eof_seen = false;
    let mut filled_since_emit: u32 = 0;
    loop {
        // 背压点：池满（前沿帧未消费）时阻塞等待；closed → None 退出
        let Some(slot) = pool.acquire_free_wait() else { return };
        // 帧获取循环：EOF → 重开解码器（同后端）继续用本槽（槽零损耗）；
        // 重开后立即 EOF（文件异常）→ 降级退出防空转
        let input = loop {
            match anim_fill_next(&mut dec, backend) {
                Ok(Some(input)) => break input,
                Ok(None) => {
                    if !eof_seen {
                        // 首个 EOF：total = 已发布帧数（0..frame_idx）
                        pool.set_total(frame_idx);
                        eof_seen = true;
                        log::info!("[动画] 全片完成池化：{} 帧", frame_idx);
                        emit_anim_fill(&app, &pool, backend_name);
                        // 全驻留（total ≤ capacity）：挂起填充线程，不重开解码器
                        //——第二轮起的循环播放由播放端从池内帧回绕完成
                        if frame_idx <= pool.capacity() as u64 {
                            log::info!(
                                "[动画] 全驻留：{} 帧 ≤ {} 槽，填充挂起（循环播放零解码）",
                                frame_idx,
                                pool.capacity()
                            );
                            drop(dec); // 释放解码器与文件缓冲（EOF 探测槽不回填）
                            pool.wait_closed(); // 池关闭时唤醒退出线程
                            return;
                        }
                    }
                    // 滑窗（total > capacity）：重开解码器续填第二轮
                    match super::decode::jxl::AnimPlayerDecoder::open(&path, backend) {
                        Ok(d) => dec = d,
                        Err(e) => {
                            log::warn!("[动画] 循环重开失败，填充降级: {}", e);
                            pool.set_degraded();
                            emit_anim_fill(&app, &pool, backend_name);
                            return;
                        }
                    }
                    // 重开后第一次解帧仍 None（上面下一轮处理）
                    // —— 哨兵防无限空转：
                    match anim_fill_next(&mut dec, backend) {
                        Ok(Some(input)) => break input,
                        Ok(None) | Err(_) => {
                            log::warn!("[动画] 重开解码器后仍无帧（文件异常），填充降级");
                            pool.set_degraded();
                            emit_anim_fill(&app, &pool, backend_name);
                            return;
                        }
                    }
                }
                Err(e) => {
                    log::warn!("[动画] 填充解码失败: {}", e);
                    pool.set_degraded();
                    emit_anim_fill(&app, &pool, backend_name);
                    return;
                }
            }
        };
        // 槽纹理直写（引擎锁内单次调用，内部自取/自还引擎锁：native = PQ16
        // 上传 + GPU 转换 dispatch，hybrid = GPU 链单次直写）
        let write = match pool.slot_tex(slot) {
            Some(t) => match &input {
                AnimFillInput::Pq16(frame, _) => {
                    crate::viewer::decode::pq16_gpu::convert_into(frame, &t.tex)
                }
                AnimFillInput::Snapshot(snap, _) => {
                    crate::viewer::decode::hybrid_gpu::hybrid_reconstruct_pq16_f16_into(
                        snap,
                        &dec.gamut(),
                        &t.tex,
                    )
                }
            },
            None => Err(format!("池槽 {} 纹理缺失", slot)),
        };
        let dur = match &input {
            AnimFillInput::Pq16(_, d) => *d,
            AnimFillInput::Snapshot(_, d) => *d,
        };
        drop(input); // PQ16 帧（~帧大）/ 快照（~220MB）及时释放
        match write {
            Ok(()) => {
                pool.publish(slot, frame_idx, dur);
                frame_idx += 1;
                filled_since_emit += 1;
                if filled_since_emit >= 30 {
                    filled_since_emit = 0;
                    emit_anim_fill(&app, &pool, backend_name);
                }
            }
            Err(e) => {
                log::warn!("[动画] GPU 池填充失败（帧 {}）: {}", frame_idx, e);
                pool.set_degraded();
                emit_anim_fill(&app, &pool, backend_name);
                return;
            }
        }
    }
}

/// 填充线程单帧获取结果（按后端分支：native = PQ16 原始帧，hybrid = 系数快照；
/// 两者都由 GPU 链直写池槽纹理）
enum AnimFillInput {
    /// native：PQ16 RGBA u16 原始帧（跳过 CPU PQ→scRGB 转换）+ 帧时长——
    /// `pq16_gpu::convert_into` 引擎锁内 LUT→色域→/80→f32tof16 直写槽纹理
    Pq16(super::decode::jxl::Pq16Frame, u32),
    /// hybrid：系数快照（GPU 链直写输入）+ 帧时长
    Snapshot(super::decode::hybrid::CoeffSnapshot, u32),
}

/// 解出下一填充输入（EOF → None；调用方重开解码器）
fn anim_fill_next(
    dec: &mut super::decode::jxl::AnimPlayerDecoder,
    backend: super::decode::jxl::AnimBackend,
) -> Result<Option<AnimFillInput>, String> {
    Ok(match backend {
        super::decode::jxl::AnimBackend::Native => {
            dec.next_frame_pq16()?.map(|(f, d)| AnimFillInput::Pq16(f, d))
        }
        super::decode::jxl::AnimBackend::Hybrid => dec
            .next_frame_snapshot()?
            .map(|(s, d)| AnimFillInput::Snapshot(s, d)),
    })
}

/// `anim://fill` 事件：填充进度（每 30 帧 / 首个 EOF / 降级时）
fn emit_anim_fill(app: &tauri::AppHandle, pool: &AnimFramePool, backend: &str) {
    let s = pool.stats();
    let state = match s.state {
        PoolState::Filling => "filling",
        PoolState::FullResident => "full_resident",
        PoolState::Degraded => "degraded",
    };
    let _ = app.emit(
        "anim://fill",
        serde_json::json!({
            "filled_frames": s.filled_frames,
            "filled_ms": s.filled_ms,
            "total_frames": pool.total_frames(),
            "state": state,
            "backend": backend,
        }),
    );
}

/// GPU 池路径窗口启动：填充线程 + Startup（首帧已入池）→ 播放窗口线程
fn spawn_anim_pool_window(
    app: tauri::AppHandle,
    p: PathBuf,
    g: AnimGpuStartup,
) -> Result<(), String> {
    // 显示器 HDR 门槛（与静态 HDR 查看同门槛；动画是 HDR 产物）
    ensure_hdr_monitor()?;
    let (w, h) = g.dec.dims();
    let first_duration_ms = g.first_duration_ms;
    let backend_name = anim_backend_name(g.backend);
    let pool = g.pool;

    // 填充线程：池槽 → 解帧（native CPU / hybrid 快照）→ 引擎锁内直写槽纹理
    // → publish → anim://fill
    let backend = g.backend;
    let pool2 = pool.clone();
    let app2 = app.clone();
    let path2 = p.clone();
    std::thread::Builder::new()
        .name("anim-fill".into())
        .spawn(move || {
            fill_gpu_pool(pool2, g.dec, path2, app2, backend);
        })
        .map_err(|e| format!("创建填充线程失败: {}", e))?;

    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let app_tb = app.clone(); // Startup 会 move app；工具栏需要自己的克隆
    let startup = Startup {
        // 池模式无 CPU 像素（全 GPU 链直写纹理）：宽高仅驱动布局
        src: HdrSource {
            width: w,
            height: h,
            data: Vec::new(),
        },
        name,
        monitors: enumerate_monitors().unwrap_or_default(),
        app,
        embedded: false,
        anim: Some(AnimStartup {
            ring: None,
            pool: Some(pool),
            first_duration_ms,
            backend: backend_name,
            path: p.to_path_buf(),
        }),
    };
    let rect = anim_window_rect();
    std::thread::Builder::new()
        .name("jietu-anim-view".into())
        .spawn(move || {
            // 高精度系统定时器：WM_TIMER 实际粒度跟随系统节拍（默认 15.6ms），
            // 60fps 帧时长 16.7ms 下抖动明显 → 提到 1ms（窗口关闭后还原）
            unsafe {
                windows::Win32::Media::timeBeginPeriod(1);
            }
            run_window(startup, rect, None);
            unsafe {
                windows::Win32::Media::timeEndPeriod(1);
            }
        })
        .map_err(|e| format!("创建动画播放线程失败: {}", e))?;
    // 悬浮工具栏（独立动画窗专属；跟随线程等 ANIM_HWND 就绪后对齐）
    spawn_anim_toolbar(&app_tb);
    log::info!(
        "[动画] GPU 常驻显存播放窗口已启动: {} ({}x{})",
        p.display(),
        w,
        h
    );
    Ok(())
}

/// 显示器 HDR 门槛校验
fn ensure_hdr_monitor() -> Result<(), String> {
    let monitor = pick_monitor(None);
    if !monitor.is_hdr() {
        return Err(format!(
            "显示器（{}）未开启 Windows HDR，无法回放 HDR 动图；请先在系统设置 → 屏幕 → HDR 中开启",
            monitor.device_name
        ));
    }
    Ok(())
}

/// 播放窗口默认几何：显示器 82% 居中（ring / 池路径共用）
fn anim_window_rect() -> RECT {
    let m = pick_monitor(None).rect();
    let mw = m.right - m.left;
    let mh = m.bottom - m.top;
    let win_w = ((mw as f32 * 0.82) as i32).max(320).min(mw);
    let win_h = ((mh as f32 * 0.82) as i32).max(240).min(mh);
    RECT {
        left: m.left + (mw - win_w) / 2,
        top: m.top + (mh - win_h) / 2,
        right: m.left + (mw - win_w) / 2 + win_w,
        bottom: m.top + (mh - win_h) / 2 + win_h,
    }
}

/// ring 路径（原流式架构）：解码线程顺序解帧推入 AnimRing，窗口线程按
/// 帧 duration 定时消费；EOF → 解码线程重开文件 → 无缝循环。首帧在命令
/// 线程同步解码（窗口纹理初始化）。
async fn open_animation_player_ring(app: tauri::AppHandle, p: PathBuf) -> Result<(), String> {
    // 1. 打开流式解码器 + 同步解首帧（窗口纹理初始化需要）
    // 后端按 config.toml [viewer] anim_backend 运行时选择（native|hybrid）
    let mut dec = super::decode::jxl::AnimPlayerDecoder::open_from_config(&p)?;
    let first = dec
        .next_frame()?
        .ok_or_else(|| "动画无有效帧".to_string())?;
    let (w, h) = dec.dims();
    let first_duration_ms = first.duration_ms;
    let src = HdrSource {
        width: w,
        height: h,
        data: first.data.clone(),
    };

    // 2. 显示器 HDR 校验（与静态 HDR 查看同门槛；动画是 HDR 产物）
    ensure_hdr_monitor()?;

    // ring 路径解码后端名（AnimStartup 结构完整性用；ring 不发 anim://fill 事件）
    let backend_name: &'static str =
        match super::decode::jxl::AnimBackend::parse(&crate::config::Config::load().viewer.anim_backend)
        {
            super::decode::jxl::AnimBackend::Hybrid => "hybrid",
            super::decode::jxl::AnimBackend::Native => "native",
        };

    // 3. 有界环形 + 解码线程（生产者；从第 1 帧起推流——第 0 帧已作纹理初始化）
    let ring = std::sync::Arc::new(AnimRing::new(AnimRing::DEFAULT_BYTES));
    let ring2 = ring.clone();
    let path2 = p.clone();
    std::thread::Builder::new()
        .name("anim-decode".into())
        .spawn(move || {
            let mut dec = dec;
            // 解码速率统计：每 60 帧打点一次（ms/帧，对照 16.7ms 实时预算）
            let mut dec_count: u32 = 0;
            let mut dec_elapsed = std::time::Instant::now();
            loop {
                match dec.next_frame() {
                    Ok(Some(frame)) => {
                        dec_count += 1;
                        if dec_count % 60 == 0 {
                            let per = dec_elapsed.elapsed().as_millis() as f64 / 60.0;
                            log::info!(
                                "[动画] 解码速率: {per:.1}ms/帧 (累计 {dec_count} 帧，实时预算 16.7ms)"
                            );
                            dec_elapsed = std::time::Instant::now();
                        }
                        if !ring2.push(frame) {
                            return; // 窗口关闭（closed）
                        }
                    }
                    Ok(None) => {
                        // EOF → 无缝循环：重开解码器从第 0 帧继续（同 config 后端）
                        match super::decode::jxl::AnimPlayerDecoder::open_from_config(&path2) {
                            Ok(d) => dec = d,
                            Err(e) => {
                                log::error!("[动画] 循环重开失败: {}", e);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("[动画] 解码线程失败: {}", e);
                        return;
                    }
                }
            }
        })
        .map_err(|e| format!("创建解码线程失败: {}", e))?;

    // 4. 窗口（独立模式；run_window 复用——Startup.anim 驱动 WM_TIMER 播放）
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let startup = Startup {
        src,
        name,
        monitors: enumerate_monitors().unwrap_or_default(),
        app: app.clone(),
        embedded: false,
        anim: Some(AnimStartup {
            ring: Some(ring),
            pool: None,
            first_duration_ms,
            backend: backend_name,
            path: p.to_path_buf(),
        }),
    };
    let rect = anim_window_rect();
    std::thread::Builder::new()
        .name("jietu-anim-view".into())
        .spawn(move || {
            // 高精度系统定时器：WM_TIMER 实际粒度跟随系统节拍（默认 15.6ms），
            // 60fps 帧时长 16.7ms 下抖动明显 → 提到 1ms（窗口关闭后还原）
            unsafe {
                windows::Win32::Media::timeBeginPeriod(1);
            }
            run_window(startup, rect, None);
            unsafe {
                windows::Win32::Media::timeEndPeriod(1);
            }
        })
        .map_err(|e| format!("创建动画播放线程失败: {}", e))?;
    // 悬浮工具栏（ring 路径同样提供；独立动画窗专属）
    spawn_anim_toolbar(&app);
    log::info!("[动画] 播放窗口已启动: {} ({}x{})", p.display(), w, h);
    Ok(())
}

// ==================== 嵌入模式命令 ====================

/// HWND 跨线程传递包装（原始指针非 Send；窗口创建前仅作句柄值搬运）
#[derive(Clone, Copy)]
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}

/// 当前嵌入子窗口句柄（0 = 无）；窗口销毁时由所属线程比对清除
static EMBEDDED_HWND: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);

/// 嵌入模式的宿主（viewer 窗口）HWND——顶层 owned 窗口的位置跟踪锚点
static PARENT_HWND: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);

/// 最新嵌入矩形（相对 viewer 客户区的 offset + 尺寸；WinEvent 钩子用它在
/// viewer 移动时同步重定位 owned 窗口）
static LAST_RECT: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);

/// 独立动画播放窗口句柄（悬浮工具栏的目标；窗口销毁时清除）
static ANIM_HWND: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);

/// 动画暂停态镜像（AtomicBool，工具栏查询用——避免跨线程读 HdrView 竞态）
static ANIM_PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 悬浮工具栏动作码（前端按钮 → 命令 → PostMessage → wnd_proc 分派）
#[allow(dead_code)]
mod toolbar_action {
    pub const TOGGLE_PAUSE: u32 = 1;
    pub const SEEK_BACK_1: u32 = 2;
    pub const SEEK_FWD_1: u32 = 3;
    pub const SEEK_BACK_3: u32 = 4;
    pub const SEEK_FWD_3: u32 = 5;
    pub const SEEK_BACK_5: u32 = 6;
    pub const SEEK_FWD_5: u32 = 7;
    pub const SEEK_BACK_10: u32 = 8;
    pub const SEEK_FWD_10: u32 = 9;
    pub const SEEK_BACK_1S: u32 = 10;
    pub const SEEK_FWD_1S: u32 = 11;
    pub const SEEK_BACK_3S: u32 = 12;
    pub const SEEK_FWD_3S: u32 = 13;
    pub const EXPORT_PNG: u32 = 14;
    pub const EXPORT_JXL: u32 = 15;
    pub const TOGGLE_FULLSCREEN: u32 = 16;
}

/// 悬浮工具栏：派发动作到独立动画播放窗口（前端 invoke 的后端命令）
#[tauri::command]
pub fn anim_toolbar_action(action: u32) {
    let hwnd = ANIM_HWND.lock().ok().and_then(|g| *g);
    let Some(h) = hwnd else {
        return;
    };
    let hwnd = HWND(h as *mut core::ffi::c_void);
    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_TOOLBAR, WPARAM(action as usize), LPARAM(0));
    }
}

/// 悬浮工具栏状态轮询（前端 250ms）：光标是否在播放窗口上 + 暂停态
#[tauri::command]
pub fn anim_toolbar_state() -> serde_json::Value {
    let hwnd = ANIM_HWND.lock().ok().and_then(|g| *g);
    let over = hwnd.is_some_and(|h| {
        let hwnd = HWND(h as *mut core::ffi::c_void);
        unsafe {
            let mut pt = POINT::default();
            if windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt).is_err() {
                return false;
            }
            // 最小化窗口不接受工具栏
            if windows::Win32::UI::WindowsAndMessaging::IsIconic(hwnd).as_bool() {
                return false;
            }
            let hovered = windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(pt);
            hovered.0 == hwnd.0
        }
    });
    serde_json::json!({
        "over": over,
        "paused": ANIM_PAUSED.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// 创建动画悬浮工具栏（独立透明置顶 webview 窗 + 跟随线程）。
/// 在池/环两种打开路径的窗口线程 spawn 后调用；重复打开时旧工具栏已随
/// 旧窗口 WM_DESTROY 销毁，此处直接建新。
fn spawn_anim_toolbar(app: &tauri::AppHandle) {
    // 已存在（快速换图竞态）→ 复用
    if app.get_webview_window("anim-toolbar").is_some() {
        return;
    }
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let window = match WebviewWindowBuilder::new(
            &app2,
            "anim-toolbar",
            tauri::WebviewUrl::App("#/anim-toolbar".into()),
        )
        .title("")
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .visible(false)
        .focused(false)
        .inner_size(660.0, 48.0) // 预设偏大；前端挂载后按内容实测微调（fitWindowToContent）
        .position(-32000.0, -32000.0)
        .build()
        {
            Ok(w) => w,
            Err(e) => {
                log::warn!("[动画] 工具栏窗口创建失败: {}", e);
                return;
            }
        };
        if let Ok(h) = window.hwnd() {
            let hwnd = HWND(h.0 as *mut core::ffi::c_void);
            unsafe {
                let style =
                    windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                        hwnd,
                        windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
                    );
                let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
                    style
                        | windows::Win32::UI::WindowsAndMessaging::WS_EX_NOACTIVATE.0 as isize,
                );
                let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
                );
            }
        }
        // 跟随线程：等 ANIM_HWND 就绪 → 每 200ms 对齐播放窗口底部居中
        let app3 = app2.clone();
        std::thread::Builder::new()
            .name("anim-toolbar-follow".into())
            .spawn(move || {
                // 等待播放窗口登记（窗口线程 run_window 设置；≤10s）
                let mut viewer: Option<isize> = None;
                for _ in 0..100 {
                    viewer = ANIM_HWND.lock().ok().and_then(|g| *g);
                    if viewer.is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let Some(viewer) = viewer else { return };
                let vh = HWND(viewer as *mut core::ffi::c_void);
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let Some(w) = app3.get_webview_window("anim-toolbar") else {
                        return; // 工具栏已销毁（播放窗口关闭）
                    };
                    unsafe {
                        if !windows::Win32::UI::WindowsAndMessaging::IsWindow(vh).as_bool() {
                            let _ = w.close();
                            return;
                        }
                        let mut rc = RECT::default();
                        if GetWindowRect(vh, &mut rc).is_err() {
                            continue;
                        }
                        if windows::Win32::UI::WindowsAndMessaging::IsIconic(vh).as_bool() {
                            continue; // 最小化：保持原位（窗口不可见）
                        }
                        let Ok(size) = w.outer_size() else { continue };
                        let vw = rc.right - rc.left;
                        let x = rc.left + (vw - size.width as i32) / 2;
                        let y = rc.bottom - size.height as i32 - 20;
                        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
                    }
                }
            })
            .ok();
    });
}

/// 着色器字节码缓存（启动预热编译；打开时零编译开销）
static SHADER_CACHE: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();

/// 启动预热：注册窗口类 + 预编译 VS/PS（后台线程调用，一次性）
///
/// D3DCompile 运行时编译是打开 HDR 视图的主要延迟来源（100-300ms），
/// 预热后 HdrView::create 直接命中缓存，打开仅剩设备/交换链创建（~20ms）。
pub fn prewarm() {
    let t0 = std::time::Instant::now();
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        register_class(hinst);
    }
    let vs = unsafe { compile_shader(VS_HLSL, b"vs_4_0\0") };
    let ps = unsafe { compile_shader(PS_HLSL, b"ps_4_0\0") };
    match (vs, ps) {
        (Ok(vs), Ok(ps)) => {
            let (vb, pb) = unsafe { (blob_bytes(&vs).to_vec(), blob_bytes(&ps).to_vec()) };
            let _ = SHADER_CACHE.set((vb, pb));
            log::info!(
                "[真HDR] 预热完成（着色器已编译并缓存，{}ms）",
                t0.elapsed().as_millis()
            );
        }
        (r1, r2) => {
            log::warn!(
                "[真HDR] 预热编译失败（打开时将回退运行时编译）: vs={:?} ps={:?}",
                r1.err(),
                r2.err()
            );
        }
    }
}

/// 矩形排队：前端矩形先于窗口就绪到达时暂存（窗口线程起来后消费）
///
/// 时序竞态修复：open_hdr_viewer spawn 线程即返回，前端 nextTick 后就发
/// set_hdr_view_rect——此时窗口线程还在 WM_CREATE 做 D3D 初始化，
/// EMBEDDED_HWND 未登记，矩形会被吞 → 窗口永远等不到首个矩形保持隐藏。
static PENDING_RECT: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);

/// 关闭旧嵌入视图（跨线程：PostMessage 到窗口所属线程，由 DefWindowProc(WM_CLOSE)
/// → DestroyWindow 在正确线程销毁）
fn close_embedded() {
    // 同时清掉残留排队矩形（防止旧矩形污染新窗口）
    if let Ok(mut p) = PENDING_RECT.lock() {
        *p = None;
    }
    let taken = EMBEDDED_HWND.lock().ok().and_then(|mut g| g.take());
    if let Some(raw) = taken {
        unsafe {
            let _ = PostMessageW(HWND(raw as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// viewer 客户区原点的屏幕坐标（嵌入 owned 窗口定位锚）
unsafe fn parent_client_origin() -> Option<POINT> {
    let raw = PARENT_HWND.lock().ok().and_then(|g| *g)?;
    let mut pt = POINT { x: 0, y: 0 };
    if !ClientToScreen(HWND(raw as *mut _), &mut pt).as_bool() {
        return None;
    }
    Some(pt)
}

/// 截图与 HDR 画布共存（设计决策）
///
/// 嵌入画布**不**因截图隐藏/关闭：DDA 采集的是 DWM 合成后的画面，
/// scRGB 画布内容会以 HDR 数值进入截图（用户要的"截到 HDR 模式"）；
/// 区域覆盖层置顶 fullscreen 在画布之上，交互不受影响。
/// 前提：画布 WS_EX_NOACTIVATE 不抢焦点，overlay.show + set_focus 垄断输入。

/// 对登记窗口应用嵌入矩形（相对客户区 → 屏幕坐标）
///
/// 画布矩形从工具栏下方开始（前端 hdr-slot top 56px），工具栏区域留给
/// WebView2（可见可点）。不用 SetWindowRgn 挖洞——窗口 region 强制 GDI
/// 重定向路径，与 flip 模型交换链不兼容。
///
/// 首次显示防闪：SetWindowPos（定位，同步分发 WM_SIZE → 内部已 render 一帧
/// 并 Present）之后再 ShowWindow——若带 SWP_SHOWWINDOW 一步完成，DWM 在
/// 后台缓冲尚无内容时即开始合成 → 初始黑/白帧闪现（"顶层闪烁"根因）。
unsafe fn apply_embedded_rect(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    let Some(anchor) = parent_client_origin() else {
        return Ok(());
    };
    // 记录相对 offset（viewer 移动时钩子重定位用）
    if let Ok(mut lr) = LAST_RECT.lock() {
        *lr = Some((x, y, w, h));
    }
    SetWindowPos(
        hwnd,
        HWND_TOP, // 压在 owner（viewer）之上；owned 语义自动维持
        anchor.x + x,
        anchor.y + y,
        w,
        h,
        SWP_NOACTIVATE,
    )
    .map_err(|e| format!("SetWindowPos: {}", e))?;
    // 定位后显示：WM_SIZE（SetWindowPos 同步分发）已渲染首帧，显示即有内容
    let _ = ShowWindow(hwnd, SW_SHOW);
    Ok(())
}

/// 前端同步容器矩形（CSS px × devicePixelRatio → viewer 客户区物理坐标）
///
/// async fn：Tauri 放线程池执行——SetWindowPos 跨线程同步等待 HDR 窗口线程，
/// 绝不能占用主线程（曾导致主线程冻死：热键/Esc/窗口切换全部无响应）。
/// 窗口就绪 → 直接应用；未就绪（D3D 初始化中）→ 排队，窗口线程登记后
/// 自行消费；无嵌入视图（独立模式/已销毁）→ 静默忽略。
#[tauri::command]
pub async fn set_hdr_view_rect(x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    if w <= 0 || h <= 0 {
        return Ok(());
    }
    let cur = EMBEDDED_HWND.lock().ok().and_then(|g| *g);
    match cur {
        Some(raw) => {
            let r = unsafe { apply_embedded_rect(HWND(raw as *mut _), x, y, w, h) };
            if r.is_ok() {
                log::info!("[真HDR] 矩形已直接应用: ({},{},{}x{})", x, y, w, h);
            }
            r
        }
        None => {
            // 窗口线程尚未登记（D3D 初始化中）：排队等窗口起来消费
            log::info!("[真HDR] 矩形先于窗口就绪，排队: ({},{},{}x{})", x, y, w, h);
            if let Ok(mut p) = PENDING_RECT.lock() {
                *p = Some((x, y, w, h));
            }
            Ok(())
        }
    }
}

/// viewer 位置/可见性跟踪钩子（嵌入 owned 窗口跟随宿主移动）
///
/// 回调铁律：只 PostMessage（钩子回调可能在系统窗口锁上下文中被分发，直接做
/// 同步窗口操作有跨线程死锁风险——曾与主线程对同一窗口的 SetWindowPos 争锁
/// 互等，把整个主线程冻死）。重定位/显隐由 HDR 窗口线程在消息循环里处理。
unsafe extern "system" fn viewer_event_hook(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 {
        return;
    }
    // 只关心 viewer 宿主窗口的事件
    let parent = PARENT_HWND.lock().ok().and_then(|g| *g);
    let Some(parent_raw) = parent else { return };
    if hwnd.0 as isize != parent_raw {
        return;
    }
    let self_raw = EMBEDDED_HWND.lock().ok().and_then(|g| *g);
    let Some(raw) = self_raw else { return };
    let hwnd_self = HWND(raw as *mut _);
    match event {
        EVENT_OBJECT_LOCATIONCHANGE => {
            let _ = PostMessageW(hwnd_self, WM_APP_REPOSITION, WPARAM(0), LPARAM(0));
        }
        EVENT_OBJECT_HIDE => {
            let _ = PostMessageW(hwnd_self, WM_APP_HIDE, WPARAM(0), LPARAM(0));
        }
        EVENT_OBJECT_SHOW => {
            let _ = PostMessageW(hwnd_self, WM_APP_SHOW, WPARAM(0), LPARAM(0));
        }
        _ => {}
    }
}

/// 钩子 → 窗口线程重定位（viewer 移动：ClientToScreen 锚点 + 相对矩形）
unsafe fn handle_reposition(hwnd: HWND) {
    let rect = LAST_RECT.lock().ok().and_then(|g| *g);
    let Some((ox, oy, w, h)) = rect else { return };
    if let Some(anchor) = parent_client_origin() {
        let _ = SetWindowPos(
            hwnd,
            None,
            anchor.x + ox,
            anchor.y + oy,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 关闭嵌入 HDR 视图（Bolt 按钮再点一次 / 切到非 HDR 图 / 离开看图模式）
#[tauri::command]
pub fn close_hdr_viewer() -> Result<(), String> {
    close_embedded();
    Ok(())
}

// ==================== 窗口线程 ====================

/// 窗口启动参数（命令线程 → 渲染线程）
struct Startup {
    src: HdrSource,
    name: String,
    monitors: Vec<MonitorInfo>,
    /// Tauri 句柄（销毁时向前端派发 hdr-view://closed）
    app: tauri::AppHandle,
    /// 嵌入模式（WS_CHILD 子窗口）标记
    embedded: bool,
    /// 动画流（Some = 动画回放模式：WM_TIMER 驱动逐帧翻页；None = 静态图）
    anim: Option<AnimStartup>,
}

/// 动画帧环形队列（解码线程生产 → 窗口线程消费）
///
/// 与录制 FrameRing 同构：字节封顶背压（解码快于播放时阻塞生产者），
/// close 后 pop 返回 None（线程退出）。播放循环 = 解码线程到 EOF 重开文件
/// 继续 push（流无限），播放端按序消费即可无缝循环。
struct AnimRing {
    inner: std::sync::Mutex<std::collections::VecDeque<super::decode::jxl::AnimFrame>>,
    cond: std::sync::Condvar,
    closed: std::sync::atomic::AtomicBool,
    max_bytes: usize,
    total_bytes: std::sync::atomic::AtomicUsize,
}

impl AnimRing {
    /// 默认 4GB（2K 帧 32.8MB ≈ 120 帧 ≈ 4s @30fps 缓冲）
    const DEFAULT_BYTES: usize = 4 * 1024 * 1024 * 1024;

    fn new(max_bytes: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::VecDeque::new()),
            cond: std::sync::Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            max_bytes: max_bytes.max(256 * 1024 * 1024),
            total_bytes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 生产者：入队（满则阻塞背压；closed 后返回 false 放弃）
    fn push(&self, frame: super::decode::jxl::AnimFrame) -> bool {
        let flen = frame.data.len();
        let mut q = self.inner.lock().unwrap();
        while self.total_bytes.load(std::sync::atomic::Ordering::Relaxed) + flen > self.max_bytes
            && q.len() >= 2
        {
            if self.closed.load(std::sync::atomic::Ordering::Acquire) {
                return false;
            }
            let (q2, _) = self
                .cond
                .wait_timeout(q, std::time::Duration::from_millis(50))
                .unwrap();
            q = q2;
        }
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        self.total_bytes
            .fetch_add(flen, std::sync::atomic::Ordering::Relaxed);
        q.push_back(frame);
        drop(q);
        self.cond.notify_all();
        true
    }

    /// 消费者：非阻塞弹出（空 = 解码滞后，播放端保持当前帧）
    fn try_pop(&self) -> Option<super::decode::jxl::AnimFrame> {
        let mut q = self.inner.lock().unwrap();
        if let Some(f) = q.pop_front() {
            self.total_bytes
                .fetch_sub(f.data.len(), std::sync::atomic::Ordering::Relaxed);
            drop(q);
            self.cond.notify_all();
            return Some(f);
        }
        None
    }

    /// 首帧等待（阻塞直到数据/closed）
    fn wait_first(&self) -> Option<super::decode::jxl::AnimFrame> {
        let mut q = self.inner.lock().unwrap();
        loop {
            if let Some(f) = q.pop_front() {
                self.total_bytes
                    .fetch_sub(f.data.len(), std::sync::atomic::Ordering::Relaxed);
                return Some(f);
            }
            if self.closed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            let (q2, _) = self
                .cond
                .wait_timeout(q, std::time::Duration::from_millis(100))
                .unwrap();
            q = q2;
        }
    }

    fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.cond.notify_all();
    }
}

const ANIM_TIMER_ID: usize = 1;
const ANIM_TIMER_MS: u32 = 5;

thread_local! {
    /// CreateWindowExW 期间 WM_CREATE 取启动参数（同线程传递，避免跨消息所有权混乱）
    static PENDING: RefCell<Option<Box<Startup>>> = const { RefCell::new(None) };
}

const CLASS_NAME: PCWSTR = w!("jietu_hdr_view_wnd");

/// 钩子 → 窗口线程消息（WM_APP 私有区间：重定位 / 隐藏 / 显示 / 工具栏）
const WM_APP_REPOSITION: u32 = WM_APP; // 0x8000
const WM_APP_HIDE: u32 = WM_APP + 1;
const WM_APP_SHOW: u32 = WM_APP + 2;
/// 悬浮工具栏动作（wp = toolbar_action 码）
const WM_APP_TOOLBAR: u32 = WM_APP + 3;

/// 窗口线程主函数：注册类 → 创建窗口（嵌入=WS_CHILD / 独立=顶层）→ 消息循环
fn run_window(startup: Startup, rect: RECT, parent: Option<HWND>) {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        register_class(hinst);

        let title = format!("真 HDR 查看 · {}", startup.name);
        let embedded = startup.embedded;
        let is_anim = startup.anim.is_some();
        let app_ready = startup.app.clone();
        PENDING.with(|p| *p.borrow_mut() = Some(Box::new(startup)));

        let (style, exstyle, hwnd_parent, x, y, w, h) = match parent {
            Some(ph) => {
                // 嵌入 = 顶层 owned 无边框窗口（owner=viewer）。
                // 不能用 WS_CHILD：flip 模型交换链（FLIP_DISCARD）在子窗口上
                // 无法被 DWM 合成（GDI 裁剪路径）→ Present 成功但画面黑屏。
                // owned 顶层窗口走标准 DWM 高级合成（与独立窗口同路径），
                // 且自带两项免费语义：永远在 owner 之上、owner 最小化时自动隐藏。
                // 移动跟随由 viewer_event_hook（WinEvent）主动跟踪。
                let mut rc = RECT::default();
                let _ = GetClientRect(ph, &mut rc);
                let cx = (rc.right - rc.left) / 2 - 50;
                let cy = (rc.bottom - rc.top) / 2 - 50;
                (
                    WINDOW_STYLE(WS_POPUP.0),
                    WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0),
                    Some(ph),
                    cx,
                    cy,
                    100,
                    100,
                )
            }
            None => (
                WS_OVERLAPPEDWINDOW,
                WINDOW_EX_STYLE(0),
                None,
                rect.left,
                rect.top,
                (rect.right - rect.left).max(320),
                (rect.bottom - rect.top).max(240),
            ),
        };

        let hwnd = CreateWindowExW(
            exstyle,
            CLASS_NAME,
            PCWSTR(to_wide(&title).as_ptr()),
            style,
            x,
            y,
            w,
            h,
            hwnd_parent.as_ref(),
            None,
            hinst,
            None,
        );

        match hwnd {
            Ok(hwnd) => {
                if embedded {
                    // 登记全局句柄 + 宿主锚点（前端 set_rect / close / 位置钩子的操作目标）
                    if let Ok(mut g) = EMBEDDED_HWND.lock() {
                        *g = Some(hwnd.0 as isize);
                    }
                    if let Some(ph) = parent.as_ref() {
                        if let Ok(mut p) = PARENT_HWND.lock() {
                            *p = Some(ph.0 as isize);
                        }
                    }
                    // viewer 位置/显隐跟踪（owned 窗口不随 owner 移动，需挂钩子）
                    // WINEVENT_OUTOFCONTEXT=0：回调投递到本线程消息循环（正在 pump）
                    let viewer_thread = GetWindowThreadProcessId(hwnd_parent.unwrap(), None);
                    let hook = SetWinEventHook(
                        EVENT_OBJECT_SHOW,
                        EVENT_OBJECT_LOCATIONCHANGE,
                        None,
                        Some(viewer_event_hook),
                        0,
                        viewer_thread,
                        0, // WINEVENT_OUTOFCONTEXT
                    );
                    if let Some(v) = view_of(hwnd) {
                        v.hook = Some(hook);
                    }
                    // 消费前端提前到达的矩形（D3D 初始化期间排队的）：
                    // CreateWindowExW 返回 = WM_CREATE 已完成（D3D 就绪），可安全定位显示
                    let pending = PENDING_RECT.lock().ok().and_then(|mut p| p.take());
                    match pending {
                        Some((px, py, pw, ph)) => {
                            log::info!("[真HDR] 消费排队矩形: ({},{},{}x{})", px, py, pw, ph);
                            if let Err(e) = apply_embedded_rect(hwnd, px, py, pw, ph) {
                                log::error!("[真HDR] 应用排队矩形失败: {}", e);
                            }
                        }
                        None => {
                            // 矩形还没到：通知前端窗口已就绪（补发矩形）
                            log::info!("[真HDR] 嵌入窗口就绪，等待前端矩形");
                            let _ = app_ready.emit_to("viewer", "hdr-view://ready", ());
                        }
                    }
                } else {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    // 独立动画播放窗：登记全局句柄（悬浮工具栏的动作目标）
                    if is_anim {
                        if let Ok(mut g) = ANIM_HWND.lock() {
                            *g = Some(hwnd.0 as isize);
                        }
                    }
                    // JXL 录制回放 = 全屏采集产物 → 自动进全屏黑布影院模式
                    //（F11 可退回窗口；静态图保持窗口模式）
                    if is_anim && view_of(hwnd).is_some() {
                        if let Some(v) = view_of(hwnd) {
                            unsafe { v.enter_fullscreen(hwnd) };
                        }
                    }
                }
                let _ = UpdateWindow(hwnd);
                let mut msg = MSG::default();
                loop {
                    let r = GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0);
                    if r.0 <= 0 {
                        break; // 0 = WM_QUIT；-1 = 错误
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            Err(e) => {
                // CreateWindowExW 失败时取回 PENDING 防泄漏；WM_CREATE 返回 -1 已由系统回滚
                let _ = PENDING.with(|p| p.borrow_mut().take());
                log::error!("[真HDR] 创建窗口失败: {}", e);
            }
        }
    }
}

/// 注册窗口类（进程内一次；重复注册返回 0 但类已存在，不影响创建）
fn register_class(hinst: HMODULE) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let hinstance: HINSTANCE = hinst.into();
        let hicon = LoadIconW(
            Some(&hinstance),
            PCWSTR(1usize as *const u16), // tauri 内嵌图标资源 ID 1
        )
        .or(LoadIconW(None, IDI_APPLICATION))
        .unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: WNDCLASS_STYLES(CS_HREDRAW.0 | CS_VREDRAW.0 | CS_DBLCLKS.0),
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: hicon,
            lpszClassName: CLASS_NAME,
            hIconSm: hicon,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            log::error!("[真HDR] RegisterClassExW 失败（若类已存在可忽略）");
        }
    });
}

fn to_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

fn get_x_lp(lp: LPARAM) -> i32 {
    ((lp.0 & 0xFFFF) as u16 as i16) as i32
}
fn get_y_lp(lp: LPARAM) -> i32 {
    (((lp.0 >> 16) & 0xFFFF) as u16 as i16) as i32
}

/// 虚拟键当前按下态（VK 常量值直传：0x10=SHIFT 0x11=CONTROL）——
/// 调帧档位的修饰键判定用
fn key_down(vk: usize) -> bool {
    unsafe {
        let s = windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(vk as i32);
        (s as u16) & 0x8000 != 0
    }
}

unsafe fn view_of(hwnd: HWND) -> Option<&'static mut HdrView> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        return None;
    }
    Some(&mut *(p as *mut HdrView))
}

// ==================== 窗口过程 ====================

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => DefWindowProcW(hwnd, msg, wp, lp),
        WM_CREATE => {
            let Some(startup) = PENDING.with(|p| p.borrow_mut().take()) else {
                log::error!("[真HDR] WM_CREATE 无启动参数");
                return LRESULT(-1);
            };
            match HdrView::create(hwnd, startup) {
                Ok(v) => {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(Box::new(v)) as isize);
                    let _ = UpdateWindow(hwnd);
                    LRESULT(0)
                }
                Err(e) => {
                    log::error!("[真HDR] D3D 初始化失败: {}", e);
                    LRESULT(-1)
                }
            }
        }
        WM_TIMER => {
            if wp.0 == ANIM_TIMER_ID {
                if let Some(v) = view_of(hwnd) {
                    v.on_anim_tick();
                }
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                let mut view = Box::from_raw(p as *mut HdrView);
                // 动画清理：停 timer + 关闭供给（ring close / pool close 唤醒
                // 填充线程退出；在屏 guard 随 AnimPlay drop 归还）
                let _ = KillTimer(hwnd, ANIM_TIMER_ID);
                if let Some(anim) = view.anim.take() {
                    if let Some(r) = anim.ring {
                        r.close();
                    }
                    if let Some(pl) = anim.pool {
                        pl.close();
                    }
                    // 文件缓存逐出：录制产物可达 GB 级，关闭播放器即归还
                    //（LRU 只在开第 3 个文件时淘汰——滞留根因）
                    super::decode::jxl::anim_file_cache_evict(&anim.path);
                }
                // 卸载 viewer 跟踪钩子（本线程安装，本线程卸载）
                if let Some(h) = view.hook.take() {
                    unsafe {
                        let _ = UnhookWinEvent(h);
                    }
                }
                // 嵌入模式：通知前端退出 HDR 布局（换图重开时前端用 switching 标记忽略）
                if view.embedded {
                    let _ = view.app.emit_to("viewer", "hdr-view://closed", ());
                }
                let app = view.app.clone();
                drop(view);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                // 独立动画窗：清全局句柄 + 暂停镜像 + 销毁悬浮工具栏
                if let Ok(mut g) = ANIM_HWND.lock() {
                    if g.map(|x| x == hwnd.0 as isize).unwrap_or(false) {
                        *g = None;
                    }
                }
                ANIM_PAUSED.store(false, std::sync::atomic::Ordering::Relaxed);
                if let Some(tb) = app.get_webview_window("anim-toolbar") {
                    let _ = tb.close();
                }
            }
            // 仅当全局登记的仍是自己才清除（防止换图重开时误清新窗口句柄）
            if let Ok(mut g) = EMBEDDED_HWND.lock() {
                if g.map(|x| x == hwnd.0 as isize).unwrap_or(false) {
                    *g = None;
                    // 会话结束：清掉宿主锚点与相对矩形（防钩子复活旧状态）
                    if let Ok(mut p) = PARENT_HWND.lock() {
                        *p = None;
                    }
                    if let Ok(mut lr) = LAST_RECT.lock() {
                        *lr = None;
                    }
                }
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mm = &mut *(lp.0 as *mut MINMAXINFO);
            // 嵌入窗口由程序定位（画布可小于 320）：放开最小尺寸限制
            let embedded = GetWindowLongPtrW(hwnd, GWLP_USERDATA) != 0
                && view_of(hwnd).map(|v| v.embedded).unwrap_or(false);
            mm.ptMinTrackSize = if embedded {
                POINT { x: 1, y: 1 }
            } else {
                POINT { x: 320, y: 240 }
            };
            LRESULT(0)
        }
        WM_APP_REPOSITION => {
            // viewer 移动：钩子投递 → 本线程重定位（同线程窗口操作，零死锁）
            handle_reposition(hwnd);
            LRESULT(0)
        }
        WM_APP_HIDE => {
            if LAST_RECT.lock().ok().map(|g| g.is_some()).unwrap_or(false) {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_APP_SHOW => {
            // 仅在会话活跃（已有矩形）时恢复显示
            if LAST_RECT.lock().ok().map(|g| g.is_some()).unwrap_or(false) {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            LRESULT(0)
        }
        WM_APP_TOOLBAR => {
            // 悬浮工具栏动作（前端按钮 → anim_toolbar_action 命令 → PostMessage）
            if let Some(v) = view_of(hwnd) {
                use toolbar_action::*;
                match wp.0 as u32 {
                    TOGGLE_PAUSE => v.toggle_pause(),
                    SEEK_BACK_1 => v.seek_frames(-1),
                    SEEK_FWD_1 => v.seek_frames(1),
                    SEEK_BACK_3 => v.seek_frames(-3),
                    SEEK_FWD_3 => v.seek_frames(3),
                    SEEK_BACK_5 => v.seek_frames(-5),
                    SEEK_FWD_5 => v.seek_frames(5),
                    SEEK_BACK_10 => v.seek_frames(-10),
                    SEEK_FWD_10 => v.seek_frames(10),
                    SEEK_BACK_1S => v.seek_seconds(-1),
                    SEEK_FWD_1S => v.seek_seconds(1),
                    SEEK_BACK_3S => v.seek_seconds(-3),
                    SEEK_FWD_3S => v.seek_seconds(3),
                    EXPORT_PNG => v.export_current_frame(false),
                    EXPORT_JXL => v.export_current_frame(true),
                    TOGGLE_FULLSCREEN => unsafe { v.toggle_fullscreen(hwnd) },
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // 背景由 D3D Clear 完成（白底）
        WM_PAINT => {
            if let Some(v) = view_of(hwnd) {
                v.render();
            }
            let _ = ValidateRect(hwnd, None);
            LRESULT(0)
        }
        WM_SIZE => {
            if wp.0 as u32 != SIZE_MINIMIZED {
                if let Some(v) = view_of(hwnd) {
                    let w = (lp.0 & 0xFFFF) as u32;
                    let h = ((lp.0 >> 16) & 0xFFFF) as u32;
                    v.on_size(hwnd, w, h);
                    // 首帧保险：显示（SWP_SHOWWINDOW→WM_SIZE）即刻直渲一帧，
                    // 不等 WM_PAINT 调度（避免首帧延迟/偶发不绘）
                    v.render();
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if let Some(v) = view_of(hwnd) {
                let mut pt = POINT {
                    x: get_x_lp(lp),
                    y: get_y_lp(lp),
                };
                let _ = ScreenToClient(hwnd, &mut pt);
                let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let factor = 1.1_f32.powf(delta as f32 / 120.0);
                v.zoom_at(pt.x as f32, pt.y as f32, factor);
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(v) = view_of(hwnd) {
                v.drag = Some((get_x_lp(lp), get_y_lp(lp)));
                let _ = SetCapture(hwnd);
                let _ = SetCursor(v.cursor_size);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(v) = view_of(hwnd) {
                let (x, y) = (get_x_lp(lp), get_y_lp(lp));
                if let Some((lx, ly)) = v.drag {
                    v.cx += (x - lx) as f32;
                    v.cy += (y - ly) as f32;
                    v.drag = Some((x, y));
                    let _ = InvalidateRect(hwnd, None, false);
                } else {
                    let _ = SetCursor(v.cursor_arrow);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(v) = view_of(hwnd) {
                v.drag = None;
                let _ = ReleaseCapture();
                let _ = SetCursor(v.cursor_arrow);
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            if let Some(v) = view_of(hwnd) {
                v.toggle_scale();
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = wp.0 & 0xFF;
            if vk == 0x1B {
                // VK_ESCAPE：全屏时先退全屏（影院模式标准 Esc 语义），再关窗
                if let Some(v) = view_of(hwnd) {
                    if v.fullscreen {
                        unsafe { v.exit_fullscreen(hwnd) };
                        return LRESULT(0);
                    }
                }
                let _ = DestroyWindow(hwnd);
            } else if vk == 0x7A {
                // VK_F11：全屏切换（嵌入窗口由前端布局控制，不支持）
                if let Some(v) = view_of(hwnd) {
                    if !v.embedded {
                        unsafe { v.toggle_fullscreen(hwnd) };
                    }
                }
            } else if vk == 0x20 {
                // VK_SPACE：暂停/恢复（仅动画）
                if let Some(v) = view_of(hwnd) {
                    v.toggle_pause();
                }
            } else if vk == 0x2C {
                // VK_SNAPSHOT（PrtSc）：导出当前帧（Ctrl = HDR JXL 单图像，
                // 默认 HDR PNG）——仅动画
                if let Some(v) = view_of(hwnd) {
                    let ctrl = key_down(0x11); // VK_CONTROL
                    v.export_current_frame(ctrl);
                }
            } else if vk == 0x25 || vk == 0x27 {
                // VK_LEFT/RIGHT：调帧（1 / Ctrl=3 / Shift=5 / Ctrl+Shift=10）
                if let Some(v) = view_of(hwnd) {
                    let dir: i64 = if vk == 0x27 { 1 } else { -1 };
                    let ctrl = key_down(0x11);
                    let shift = key_down(0x10);
                    let step: i64 = match (ctrl, shift) {
                        (false, false) => 1,
                        (true, false) => 3,
                        (false, true) => 5,
                        (true, true) => 10,
                    };
                    v.seek_frames(dir * step);
                }
            } else if vk == 0x21 || vk == 0x22 {
                // VK_PRIOR/NEXT（PgUp/PgDn）：按秒调帧（1s / Shift=3s；
                // PgDn = 前进，PgUp = 后退）
                if let Some(v) = view_of(hwnd) {
                    let dir: i64 = if vk == 0x22 { 1 } else { -1 };
                    let secs: i64 = if key_down(0x10) { 3 } else { 1 };
                    v.seek_seconds(dir * secs);
                }
            }
            LRESULT(0)
        }
        WM_MOVE => {
            if let Some(v) = view_of(hwnd) {
                v.refresh_display_state(hwnd);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            if let Some(v) = view_of(hwnd) {
                v.monitors = enumerate_monitors().unwrap_or_default();
                v.refresh_display_state(hwnd);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ==================== D3D11 渲染核心 ====================

/// 顶点：NDC 四边形 + UV（TRIANGLESTRIP 4 顶点）
#[repr(C)]
#[derive(Clone, Copy)]
struct Vtx {
    pos: [f32; 2],
    uv: [f32; 2],
}

/// 常量缓冲（16 字节对齐）：图像半尺寸（NDC）+ 图像中心（NDC）
#[repr(C)]
#[derive(Clone, Copy)]
struct QuadParams {
    half: [f32; 2],
    center: [f32; 2],
}

const VS_HLSL: &[u8] = br#"
cbuffer Params : register(b0)
{
    float2 halfSize;
    float2 center;
};
struct VSIn { float2 pos : POSITION; float2 uv : TEXCOORD; };
struct PSIn { float4 pos : SV_POSITION; float2 uv : TEXCOORD; };
PSIn main(VSIn i)
{
    PSIn o;
    o.pos = float4(i.pos * halfSize + center, 0.0, 1.0);
    o.uv = i.uv;
    return o;
}
"#;

const PS_HLSL: &[u8] = br#"
Texture2D<float4> tex : register(t0);
SamplerState samp : register(s0);
struct PSIn { float4 pos : SV_POSITION; float2 uv : TEXCOORD; };
float4 main(PSIn i) : SV_Target
{
    return tex.Sample(samp, i.uv);
}
"#;

/// 渲染状态（全部对象仅在窗口线程创建/使用）
struct HdrView {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain,
    rtv: Option<windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView>,
    srv: ID3D11ShaderResourceView,
    sampler: ID3D11SamplerState,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    vb: ID3D11Buffer,
    cbuf: ID3D11Buffer,

    img_w: u32,
    img_h: u32,
    cw: u32,
    ch: u32,
    /// 自适应模式（窗口缩放时跟随）
    fit: bool,
    /// 非自适应时的绝对缩放（1.0 = 像素 1:1）
    zoom: f32,
    /// 图像中心在客户区的像素坐标
    cx: f32,
    cy: f32,
    drag: Option<(i32, i32)>,
    cursor_arrow: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
    cursor_size: windows::Win32::UI::WindowsAndMessaging::HCURSOR,

    name: String,
    monitors: Vec<MonitorInfo>,
    cur_hdr: bool,
    cur_max_lum: f32,
    /// Tauri 句柄（销毁通知）+ 嵌入模式标记（子窗口无标题栏，跳过 SetWindowTextW）
    app: tauri::AppHandle,
    embedded: bool,
    /// viewer 位置跟踪钩子（嵌入模式；WM_DESTROY 时卸载）
    hook: Option<HWINEVENTHOOK>,
    /// 诊断：源图像中心像素 scRGB f16（创建时采样，首帧日志打印）
    center_px: [u8; 8],
    /// 白底亮度（scRGB 单位）：系统 SDR 白电平 / 80。
    /// Clear(1,1,1) 语义是 80nits 参考白，DWM 按"SDR 内容亮度"滑块映射——
    /// 滑块偏低时白底仅 ~150nits，比图像内容的白（截图屏幕白直通）暗 → 观感发灰。
    /// 改为读实际 SDR 白：白底与图像白、与 SDR 模式白底严格同亮。
    bg_white: f32,
    /// 源纹理（静态图：初始化直传；ring 动画：WM_TIMER 逐帧 UpdateSubresource
    /// 翻页；GPU 池模式 None——纹理常驻池中，SRV 随在屏 guard 换绑）
    tex: Option<ID3D11Texture2D>,
    /// 动画回放状态（None = 静态图）
    anim: Option<AnimPlay>,
    /// 全屏态（JXL 录制回放 = 全屏黑布影院模式）
    ///
    /// 进入：保存窗口样式/矩形 → WS_POPUP 无边框铺满所在显示器 + 背景转纯黑
    /// （letterbox 黑边）；退出：还原样式/矩形 + 白底。
    /// F11 切换；Esc 全屏时先退全屏再关窗；动画（JXL 录制产物）打开自动进入。
    fullscreen: bool,
    /// 全屏前的窗口状态 (style, exstyle, rect)——退出还原用
    fs_saved: Option<(isize, isize, RECT)>,
}

/// 动画回放运行态
struct AnimPlay {
    /// ring 路径帧队列（GPU 池模式 None）
    ring: Option<std::sync::Arc<AnimRing>>,
    /// GPU 常驻显存池（池模式 Some）
    pool: Option<std::sync::Arc<AnimFramePool>>,
    /// 池模式已取出帧 guard 集合（尾元素 = 在屏帧）：
    /// - 全驻留（total ≤ capacity，填充挂起）：首轮逐帧累积持有——槽位不
    ///   归还则填充永不复用改写，guard 集即完整循环缓存（回绕的前提）；
    /// - 滑窗确认（total > capacity）：仅留在屏 1 帧，换帧即归还（原语义，
    ///   保持填充前瞻）。
    held: Vec<GpuFrame<PoolTex>>,
    /// 池模式下一帧全局序号（首帧 0 在屏 → next = 1）
    next_frame_idx: u64,
    /// 全驻留循环缓存（回绕时由 held + 牺牲帧回收构建）：Some 后播放完全
    /// 走本地 SRV 直取，零解码、零池交互、零锁竞争
    resident_loop: Option<Vec<(PoolTex, u32)>>,
    /// 全驻留循环游标（resident_loop 内下标，逐帧取模回绕）
    loop_idx: usize,
    /// 滑窗确认标志：total > capacity 时置位（回绕无望，恢复换帧即归还）
    sliding: bool,
    /// 前沿饥饿牺牲记录 (frame_idx, slot_idx, duration_ms)：total 未知时
    /// 填充饥饿（阻塞等槽到不了 EOF 探测）须归还最旧 guard 解锁——其纹理
    /// 留在槽内且全驻留下填充挂起零改写，回绕时经 slot_tex 回收（仅
    /// total == capacity 边角会发生；滑窗下记录作废不用）
    sacrificed: Vec<(u64, usize, u32)>,
    /// 下一帧理论到期时刻（绝对时钟调度）
    ///
    /// 取帧成功后 `next_due += dur` 累积推进——填充抖动导致帧迟到时，下一帧
    /// 立即到期连播追回（而非把迟到量累积进总时长）。若已落后超过 2 帧时长
    /// （长时间阻塞后恢复），重置基准为 now + dur 防视觉快进冲击。
    /// 取代旧 remain_ms + last_tick 滞后模型：旧模型 last_tick = 实际取帧
    /// 时刻，每次等填充的延迟都计入下一帧周期永不追回 → 651 帧实测多播
    /// ~3.4s（15.6s 内容播 19s）。
    next_due: Instant,
    /// 暂停态（空格切换）：暂停时 tick 冻结调度（不推进 next_due 不换帧），
    /// 恢复时 next_due = now + 1 帧时长（防暂停时长累积导致恢复快进）
    paused: bool,
    /// 当前显示的**文件帧号**（绝对，0 基）——seek 数学用：
    /// tick 逐帧 +1 并在 abs_total 处回绕；resident_loop = session_base+下标
    cur_frame: u64,
    /// 文件总帧数（首次 EOF 得知；seek 边界钳制用）。重启会话后保持不覆盖
    ///（重启后池的 total 是会话帧数 = T - base，非文件总数）
    abs_total: Option<u64>,
    /// 当前会话 seq 0 对应的文件帧号（首次打开 = 0；seek 重启后 = target）
    session_base: u64,
    /// 解码后端名（anim://fill 事件 backend 字段）
    backend: &'static str,
    /// 文件路径（窗口销毁时逐出 ANIM_FILE_CACHE 条目，归还 GB 级缓存）
    path: PathBuf,
}

/// 动画启动参数（ring：首帧已在命令线程解码为 Startup.src；pool：首帧
/// idx=0 已 GPU 直写入池，Startup.src 仅宽高）
struct AnimStartup {
    ring: Option<std::sync::Arc<AnimRing>>,
    /// GPU 常驻显存池（Some = 池模式）
    pool: Option<std::sync::Arc<AnimFramePool>>,
    /// 首帧显示时长（ms）
    first_duration_ms: u32,
    /// 解码后端名（anim://fill 事件 backend 字段）
    backend: &'static str,
    /// 文件路径（销毁时逐出文件缓存）
    path: PathBuf,
}

/// 全驻留回绕缓存构建：held（尾部帧 guard，帧号 [total-held.len(), total)）
/// + sacrificed（头部牺牲帧记录，帧号 [0, sacrificed.len())——guard 归还后
/// 纹理留在槽内，全驻留下填充挂起零改写，slot_tex 直接回收）→ 本地纹理
/// 句柄数组（PoolTex COM clone = AddRef，零 GPU 内存增量）。
///
/// 计数不符或槽纹理缺失（理论不可达：首轮 guard 仅经牺牲路径归还且全记录）
/// → None，上层保持当前帧。
fn build_resident_cache(
    held: &[GpuFrame<PoolTex>],
    sacrificed: &[(u64, usize, u32)],
    pool: &AnimFramePool,
    total: u64,
) -> Option<Vec<(PoolTex, u32)>> {
    if total == 0 || held.len() as u64 + sacrificed.len() as u64 != total {
        return None;
    }
    let mut cache: Vec<Option<(PoolTex, u32)>> = (0..total).map(|_| None).collect();
    let base = total - held.len() as u64;
    for (i, g) in held.iter().enumerate() {
        cache[(base + i as u64) as usize] = Some((g.tex().clone(), g.duration_ms));
    }
    for &(f, slot, dur) in sacrificed {
        *cache.get_mut(f as usize)? = Some((pool.slot_tex(slot)?, dur));
    }
    cache.into_iter().collect()
}

impl HdrView {
    /// 动画 tick（WM_TIMER，10ms）：当前帧时长耗尽 → 取下一帧 → 换帧 → 重绘
    ///
    /// GPU 池模式三分支：
    /// 1. **全驻留循环**（resident_loop 有值）：本地纹理句柄缓存直取 SRV——
    ///    零解码、零池交互、零锁竞争（修复全驻留后仍卡顿的播放端一半）；
    /// 2. **滑窗确认**（sliding，total > capacity）：换帧即归还上一帧 guard
    ///    （槽位重回 free，填充复用显存）→ 换绑新 SRV → render——原语义；
    /// 3. **首轮累积**（total 未知或全驻留未回绕）：guard 全持有不归还
    ///    （槽位不回 free → 填充永不复用改写 → 首轮帧内容零损坏，这是
    ///    回绕缓存正确性的前提）。未命中时：total 已知且 next ≥ total 且
    ///    total ≤ capacity → **全驻留回绕**（held + 牺牲帧回收构建本地
    ///    循环缓存）；填充饥饿（producer_starved，EOF 探测被槽位阻塞）→
    ///    牺牲最旧 guard 解锁（纹理留槽内，回绕时回收）；否则保持当前帧
    ///    （填充前沿解码中，自然缓冲，tick 继续轮询）。
    /// ring 模式：try_pop 解码滞后时保持当前帧。
    fn on_anim_tick(&mut self) {
        let Some(anim) = self.anim.as_mut() else {
            return;
        };
        let now = Instant::now();
        if now < anim.next_due {
            return; // 未到理论到期时刻
        }
        if anim.paused {
            return; // 暂停：冻结调度（不换帧不推进 next_due；恢复时重置基准）
        }
        if let Some(pool) = anim.pool.clone() {
            // 文件总帧数捕获（仅原生会话：session_base==0；seek 重启后池 total
            // 是会话帧数 = T-base，不得覆盖 abs_total）
            if anim.abs_total.is_none() && anim.session_base == 0 {
                if let Some(t) = pool.total_frames() {
                    anim.abs_total = Some(t);
                }
            }
            // 1. 全驻留循环：本地缓存直取（SRV 句柄 COM clone），零池交互
            if let Some(cache) = anim.resident_loop.as_ref() {
                if cache.is_empty() {
                    return;
                }
                let (tex, dur) = &cache[anim.loop_idx];
                let dur = (*dur).max(1);
                let next_idx = (anim.loop_idx + 1) % cache.len();
                self.srv = tex.srv.clone();
                // 当前显示帧 = 会话基 + 缓存下标（缓存按帧序排列）
                anim.cur_frame = anim.session_base + anim.loop_idx as u64;
                anim.advance_schedule(now, dur);
                anim.loop_idx = next_idx;
                unsafe { self.render() };
                return;
            }

            // 滑窗确认（total 已知且 > capacity）：回绕无望——释放历史 guard
            //（仅留在屏帧），恢复"换帧即归还"原语义，填充前瞻随即恢复
            if !anim.sliding {
                if let Some(total) = pool.total_frames() {
                    if total > pool.capacity() as u64 {
                        anim.sliding = true;
                        if let Some(cur) = anim.held.pop() {
                            anim.held.clear();
                            anim.held.push(cur);
                        }
                    }
                }
            }

            match pool.get(anim.next_frame_idx) {
                Some(g) => {
                    let dur = g.duration_ms.max(1);
                    // total 未知时命中 ≥ capacity 帧 ⟹ 填充已发布超容量帧
                    // ⟹ total > capacity（全驻留下填充早已 EOF set_total）→
                    // 滑窗确认，同上释放历史 guard
                    if !anim.sliding
                        && pool.total_frames().is_none()
                        && anim.next_frame_idx >= pool.capacity() as u64
                    {
                        anim.sliding = true;
                        if let Some(cur) = anim.held.pop() {
                            anim.held.clear();
                            anim.held.push(cur);
                        }
                    }
                    if anim.sliding {
                        // 原语义：上一帧 guard Drop 归还（在引擎锁之前完成——
                        // 固定"池锁→引擎锁"顺序，绝不反向；旧帧已不在屏）
                        anim.held.clear();
                    }
                    anim.held.push(g);
                    self.srv = anim.held.last().unwrap().tex().srv.clone();
                    anim.next_frame_idx += 1;
                    // 文件帧号推进（EOF 回绕：连续播放时 T-1 → 0 自然衔接第二轮）
                    anim.cur_frame += 1;
                    if let Some(t) = anim.abs_total {
                        if anim.cur_frame >= t {
                            anim.cur_frame = 0;
                        }
                    }
                    anim.advance_schedule(now, dur);
                    unsafe { self.render() };
                }
                None => {
                    if let Some(total) = pool.total_frames() {
                        if anim.next_frame_idx >= total && total <= pool.capacity() as u64 {
                            // 2. 全驻留回绕：held（尾部帧）+ sacrificed（头部
                            // 牺牲帧，槽纹理经 EOF 探测后填充挂起零改写）→
                            // 本地循环缓存；此后零解码零读盘循环播放
                            if let Some(cache) = build_resident_cache(
                                &anim.held,
                                &anim.sacrificed,
                                &pool,
                                total,
                            ) {
                                log::info!(
                                    "[动画] 全驻留回绕：{} 帧本地缓存循环（零解码）",
                                    cache.len()
                                );
                                anim.held.clear(); // 释放全部 guard（缓存持纹理引用）
                                anim.sacrificed.clear();
                                anim.resident_loop = Some(cache);
                                anim.loop_idx = 0;
                            }
                            // 缓存构建失败（计数不符，理论不可达）→ 保持当前帧
                            return;
                        }
                        // next < total（填充前沿解码中）或滑窗第二轮（填充已
                        // 重开，全局 idx 单调续填）→ 保持当前帧，tick 继续轮询
                        return;
                    }
                    // total 未知 + 填充饥饿：EOF 探测被"无空闲槽"阻塞（仅
                    // total == capacity 边角，填充发布满全部槽后卡在取槽）→
                    // 牺牲最旧 guard 解锁：其纹理留在槽内，填充取槽后 EOF →
                    // set_total → 挂起（零写入）→ 回绕时经 slot_tex 回收。
                    // 滑窗下填充取槽后续填会改写该槽，记录自然作废。
                    if pool.producer_starved() && !anim.held.is_empty() {
                        let g = anim.held.remove(0);
                        anim.sacrificed
                            .push((g.frame_idx, g.slot(), g.duration_ms));
                        // guard Drop → 槽位归还 + notify → 填充线程唤醒取槽
                    }
                    // 保持当前帧（填充前沿未就绪）
                }
            }
            return;
        }
        let Some(frame) = anim.ring.as_ref().and_then(|r| r.try_pop()) else {
            return; // 解码滞后：保持当前帧
        };
        if let Some(tex) = self.tex.as_ref() {
            unsafe {
                self.ctx.UpdateSubresource(
                    tex,
                    0,
                    None,
                    frame.data.as_ptr() as *const core::ffi::c_void,
                    self.img_w * 8,
                    0,
                );
            }
        }
        anim.advance_schedule(now, frame.duration_ms.max(1));
        unsafe { self.render() };
    }
}

impl AnimPlay {
    /// 绝对时钟推进（取帧成功后调用）：next_due += dur 累积理论到期；
    /// 已落后超过 2 帧时长（长阻塞后恢复）→ 重置基准 now + dur 防快进冲击
    fn advance_schedule(&mut self, now: Instant, dur_ms: u32) {
        let dur = std::time::Duration::from_millis(dur_ms.max(1) as u64);
        let due = self.next_due + dur;
        self.next_due = if due + dur < now { now + dur } else { due };
    }
}

impl HdrView {
    /// 进入全屏：WS_POPUP 铺满所在显示器 + 黑背景（JXL 录制回放影院模式）
    unsafe fn enter_fullscreen(&mut self, hwnd: HWND) {
        if self.fullscreen || self.embedded {
            return;
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        self.fs_saved = Some((style, exstyle, rc));
        // 无边框：保留可见性位，去装饰
        let _ = SetWindowLongPtrW(
            hwnd,
            GWL_STYLE,
            (WS_POPUP | WS_VISIBLE).0 as isize,
        );
        // 铺满所在显示器（整屏物理矩形，含任务栏区域）
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
            ..Default::default()
        };
        if windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi).as_bool() {
            let _ = SetWindowPos(
                hwnd,
                None,
                mi.rcMonitor.left,
                mi.rcMonitor.top,
                mi.rcMonitor.right - mi.rcMonitor.left,
                mi.rcMonitor.bottom - mi.rcMonitor.top,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        self.fullscreen = true;
        self.apply_fit(); // letterbox 居中
        self.render();
        log::info!("[真HDR] 进入全屏（黑布影院模式）");
    }

    /// 退出全屏：还原窗口样式/矩形 + 白底
    unsafe fn exit_fullscreen(&mut self, hwnd: HWND) {
        if !self.fullscreen {
            return;
        }
        if let Some((style, exstyle, rc)) = self.fs_saved.take() {
            let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, exstyle);
            let _ = SetWindowPos(
                hwnd,
                None,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        self.fullscreen = false;
        self.apply_fit();
        self.render();
        log::info!("[真HDR] 退出全屏");
    }

    unsafe fn toggle_fullscreen(&mut self, hwnd: HWND) {
        if self.fullscreen {
            self.exit_fullscreen(hwnd);
        } else {
            self.enter_fullscreen(hwnd);
        }
    }

    /// 空格暂停/恢复：暂停时冻结调度；恢复时重置 next_due = now + 帧时长
    ///（防暂停时长被绝对时钟当作欠账、恢复瞬间连播快进）
    fn toggle_pause(&mut self) {
        let Some(anim) = self.anim.as_mut() else {
            return;
        };
        anim.paused = !anim.paused;
        ANIM_PAUSED.store(anim.paused, std::sync::atomic::Ordering::Relaxed); // 工具栏镜像
        if !anim.paused {
            // 恢复：当前帧剩余时长未知，直接给 1 个帧时长（24ms@60fps）
            // —— 绝对时钟从这里重新起算，无累积误差
            let dur = current_frame_duration(anim).max(1);
            anim.next_due = Instant::now()
                + std::time::Duration::from_millis(dur as u64);
        }
        log::info!("[动画] {}", if anim.paused { "暂停" } else { "恢复播放" });
    }

    /// 导出当前帧（PrtSc = HDR PNG；Ctrl+PrtSc = HDR JXL 单图像）
    ///
    /// 在屏纹理（resident_loop 缓存 / held 尾部 guard）经 staging 读回
    /// scRGB f16 → CapturedTexture(R16G16B16A16_FLOAT) → 复用截图编码管线。
    /// 文件名 = 原文件名 + _frame<文件帧号> + 时间戳，保存到图片目录。
    fn export_current_frame(&mut self, as_jxl: bool) {
        let Some(anim) = self.anim.as_ref() else {
            return;
        };
        // 文件帧号（seek 后即精准帧号；ring 模式无全局序号退化为会话序号）
        let seq = if anim.session_base == 0 && anim.abs_total.is_none() {
            anim.next_frame_idx.saturating_sub(1)
        } else {
            anim.cur_frame
        };
        // 1. 找到在屏纹理：resident_loop——显示的是 (loop_idx-1)（tick 先显示
        //    后推进，loop_idx 指向下一帧）；池模式 = held 尾部 guard
        let tex: Option<ID3D11Texture2D> = if let Some(cache) = anim.resident_loop.as_ref() {
            if cache.is_empty() {
                None
            } else {
                let idx = (anim.loop_idx + cache.len() - 1) % cache.len();
                cache.get(idx).map(|(t, _)| t.tex.clone())
            }
        } else {
            anim.held.last().map(|g| g.tex().tex.clone())
        };
        let Some(tex) = tex else {
            log::warn!("[动画] 导出失败：无在屏帧纹理");
            return;
        };
        // 2. staging 读回（GPU→CPU；与渲染线程共用 immediate context——本函数
        //    在窗口线程 = 渲染线程调用，天然串行无并发）
        let read = unsafe { readback_texture(&self.device, &self.ctx, &tex, self.img_w, self.img_h) };
        let Ok(data) = read else {
            log::warn!("[动画] 导出失败：staging 读回失败: {:?}", read.err());
            return;
        };
        // 3. 构造 CapturedTexture → 复用截图编码管线
        let captured = crate::capture::dxgi_duplication::CapturedTexture {
            width: self.img_w,
            height: self.img_h,
            format: crate::color::PixelFormat::R16g16b16a16Float,
            row_pitch: self.img_w as usize * 8,
            data,
            via_gdi: false,
        };
        // 4. 输出路径：图片目录 / 原名_frame<N>_<时间戳>.{png|jxl}
        let dir = crate::config::Config::load().resolved_save_dir();
        let stem = anim
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "anim_frame".into());
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let path = dir.join(format!(
            "{stem}_frame{seq}_{ts}.{}",
            if as_jxl { "jxl" } else { "png" }
        ));
        let r = if as_jxl {
            crate::encode::jxl::save_jxl(
                &captured,
                &path,
                &crate::capture::hdr_pipeline::HdrToSdrParams::default(),
                crate::encode::QualityLevel::High,
                16,
            )
        } else {
            crate::encode::png::save_hdr_png(
                &captured,
                &path,
                &crate::capture::hdr_pipeline::HdrToSdrParams::default(),
            )
        };
        match r {
            Ok(()) => log::info!("[动画] 当前帧（文件帧 {seq}）已导出: {}", path.display()),
            Err(e) => log::error!("[动画] 导出失败: {:#}", e),
        }
    }

    /// seek：相对当前帧移动 delta 帧（负 = 后退）。档位由按键映射给出
    ///（1/3/5/10 帧、1s、3s）。
    ///
    /// 三路径：① resident_loop 全驻留 → 缓存直索引（零成本）；② 池窗口内
    /// 命中 → get + 换帧（零成本）；③ 窗口外（滑窗历史已回收 / 前沿未填）
    /// → 重启填充会话（open_at 定位，数百 ms）。
    fn seek_frames(&mut self, delta: i64) {
        let Some(anim) = self.anim.as_mut() else {
            return;
        };
        let cur = anim.cur_frame as i64;
        // 边界钳制需要文件总帧数（未得知时仅支持池窗口内命中）
        let Some(total) = anim.abs_total else {
            log::warn!("[动画] seek：文件总帧数未知（填充未完成），暂不可调帧");
            return;
        };
        let target = (cur + delta).clamp(0, total as i64 - 1) as u64;
        if target == anim.cur_frame {
            return; // 已在边界
        }
        log::info!(
            "[动画] seek: 帧 {} → {}（Δ{}，总 {}）",
            anim.cur_frame,
            target,
            delta,
            total
        );

        // ① resident_loop：缓存持有 [session_base, session_base+len)
        if let Some(cache) = anim.resident_loop.as_ref() {
            let base = anim.session_base;
            let len = cache.len() as u64;
            if target >= base && target < base + len {
                let idx = (target - base) as usize;
                let (tex, dur) = &cache[idx];
                self.srv = tex.srv.clone();
                anim.cur_frame = target;
                anim.loop_idx = (idx + 1) % cache.len();
                anim.next_due = Instant::now()
                    + std::time::Duration::from_millis((*dur).max(1) as u64);
                unsafe { self.render() };
                return;
            }
            // 缓存外（重启过的全驻留只持尾段）→ 走重启
        }

        // ② 池窗口内直接命中：seq = 当前 seq - 当前帧号 + 目标帧号（同轮换算）
        if let Some(pool) = anim.pool.clone() {
            let cur_seq = anim.next_frame_idx.saturating_sub(1);
            let target_seq = cur_seq - anim.cur_frame + target;
            if let Some(g) = pool.get(target_seq) {
                let dur = g.duration_ms.max(1);
                if anim.sliding {
                    anim.held.clear();
                }
                anim.held.push(g);
                self.srv = anim.held.last().unwrap().tex().srv.clone();
                anim.next_frame_idx = target_seq + 1;
                anim.cur_frame = target;
                anim.next_due =
                    Instant::now() + std::time::Duration::from_millis(dur as u64);
                unsafe { self.render() };
                return;
            }
            // ③ 未命中：滑窗历史已回收（后退）/ 前沿未填（大幅前进）→ 重启会话
            self.restart_pool_at(target);
        }
    }

    /// 按秒 seek（1s / 3s 档位）：换算为帧数后走 [`Self::seek_frames`]
    fn seek_seconds(&mut self, secs: i64) {
        let Some(anim) = self.anim.as_ref() else {
            return;
        };
        let dur = current_frame_duration(anim).max(1) as i64;
        let frames = (secs * 1000 / dur).max(1) * secs.signum();
        self.seek_frames(frames);
    }

    /// 重启填充会话：从文件帧 `target` 重新建池填充（滑窗后退 / 越前沿前进）
    ///
    /// 旧池 close（填充线程退出）→ 新池（同尺寸同容量）→ open_at(target)
    /// 解首帧入池 seq 0 → 区间分工填充从 target+1 起。seq 重计（会话基 =
    /// target），播放端 cur_frame/next_frame_idx 同步重置；abs_total 保持。
    /// 仅 native 后端（open_at 语义）；hybrid 不支持 seek 重启。
    fn restart_pool_at(&mut self, target: u64) {
        let Some(anim) = self.anim.as_mut() else {
            return;
        };
        if anim.backend != "native" {
            log::warn!("[动画] seek 重启仅支持 native 后端（当前 {}）", anim.backend);
            return;
        }
        let path = anim.path.clone();
        let abs_total = anim.abs_total;
        // 旧池参数（close 前取）
        let (dims, capacity) = anim
            .pool
            .as_ref()
            .map(|p| (p.dims(), p.capacity()))
            .unwrap_or(((self.img_w, self.img_h), ANIM_POOL_MIN_CAPACITY.max(3)));
        // 1. 关旧池 + 清空持有（guard 归还；填充线程经 closed 链退出）
        if let Some(p) = anim.pool.take() {
            p.close();
        }
        anim.held.clear();
        anim.sacrificed.clear();
        anim.resident_loop = None;
        anim.sliding = false;
        // 2. 填充线程退出让路（close 唤醒 → worker send 失败/publisher 取槽
        //    None 退出；百余 ms 级，旧池显存随后释放）
        std::thread::sleep(std::time::Duration::from_millis(150));
        // 3. 新池（同 dims/capacity，预分配失败则放弃 seek）
        let eng = match d3d11::engine() {
            Ok(e) => e,
            Err(e) => {
                log::error!("[动画] seek 重启：GPU 引擎不可用: {}", e);
                return;
            }
        };
        let pool = {
            let g = eng.0.lock().ok();
            let device = g.map(|g| g.device_ctx().0.clone());
            let Some(device) = device else {
                log::error!("[动画] seek 重启：GPU 引擎锁失败");
                return;
            };
            match AnimFramePool::new(
                dims,
                capacity,
                crate::viewer::anim_pool::d3d_factory(device, dims),
            ) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("[动画] seek 重启：纹理池创建失败: {}", e);
                    return;
                }
            }
        };
        // 4. 首帧（文件帧 target）解码 → GPU 直写槽 0 → publish(seq 0)
        let mut dec = match super::decode::jxl::AnimationDecoder::open_at(&path, target) {
            Ok(d) => d,
            Err(e) => {
                log::error!("[动画] seek 重启：open_at({}) 失败: {}", target, e);
                return;
            }
        };
        let (frame, dur) = match dec.next_frame_pq16() {
            Ok(Some(f)) => f,
            _ => {
                log::error!("[动画] seek 重启：帧 {} 解码失败", target);
                return;
            }
        };
        drop(dec);
        let slot = match pool.acquire_free() {
            Some(s) => s,
            None => {
                log::error!("[动画] seek 重启：池无空闲槽");
                return;
            }
        };
        let write = pool
            .slot_tex(slot)
            .map(|t| crate::viewer::decode::pq16_gpu::convert_into(&frame, &t.tex))
            .unwrap_or_else(|| Err("池槽纹理缺失".into()));
        if let Err(e) = write {
            log::error!("[动画] seek 重启：首帧 GPU 直写失败: {}", e);
            return;
        }
        pool.publish(slot, 0, dur);
        // 5. 播放态重置（保持 paused；seq 会话重计）
        let Some(anim) = self.anim.as_mut() else { return };
        anim.pool = Some(std::sync::Arc::clone(&pool));
        anim.next_frame_idx = 1;
        anim.cur_frame = target;
        anim.session_base = target;
        anim.abs_total = abs_total; // 文件总帧数保持（池 total 变为会话帧数）
        anim.next_due =
            Instant::now() + std::time::Duration::from_millis(dur.max(1) as u64);
        if let Some(g) = pool.get(0) {
            anim.held.push(g);
            self.srv = anim.held.last().unwrap().tex().srv.clone();
        }
        unsafe { self.render() };
        // 6. 填充线程：区间分工从 target+1 起（跳过探测——已确认独立帧）
        let physical_cores = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).max(1))
            .unwrap_or(1);
        let workers = physical_cores.clamp(1, 4);
        if workers > 1 {
            std::env::set_var("JXL_PLAY_WORKERS", workers.to_string());
            let app = self.app.clone();
            std::thread::Builder::new()
                .name("anim-fill".into())
                .spawn(move || {
                    fill_gpu_pool_parallel(
                        &pool,
                        &path,
                        &app,
                        super::decode::jxl::AnimBackend::Native,
                        "native",
                        workers,
                        target,
                    );
                })
                .ok();
        } else {
            // 单核：回退单线程顺序填充（从 target 起——dec 已消耗，重开）
            let app = self.app.clone();
            let pool2 = std::sync::Arc::clone(&pool);
            std::thread::Builder::new()
                .name("anim-fill".into())
                .spawn(move || {
                    fill_gpu_pool_single_seek(pool2, path, app, target);
                })
                .ok();
        }
        log::info!("[动画] seek 重启完成：会话基帧 {}（{} worker 续填）", target, workers);
    }
}

/// seek 重启的单线程填充回退（单核机器）：从文件帧 start+1 顺序解到 EOF
///（帧 start 已同步入池 seq 0）→ set_total → 滑窗第二轮从文件帧 0 续填。
fn fill_gpu_pool_single_seek(
    pool: std::sync::Arc<AnimFramePool>,
    path: PathBuf,
    app: tauri::AppHandle,
    start: u64,
) {
    let mut dec = match super::decode::jxl::AnimationDecoder::open_at(&path, start + 1) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[动画] seek 单线程填充打开失败: {}", e);
            return;
        }
    };
    let mut next_idx: u64 = 1;
    let mut filled_since_emit: u32 = 0;
    let mut total_set = false;
    loop {
        match dec.next_frame_pq16() {
            Ok(Some((frame, dur))) => {
                let Some(slot) = pool.acquire_free_wait() else { return };
                let write = pool
                    .slot_tex(slot)
                    .map(|t| crate::viewer::decode::pq16_gpu::convert_into(&frame, &t.tex))
                    .unwrap_or_else(|| Err("池槽纹理缺失".into()));
                drop(frame);
                match write {
                    Ok(()) => {
                        pool.publish(slot, next_idx, dur);
                        next_idx += 1;
                        filled_since_emit += 1;
                        if filled_since_emit >= 30 {
                            filled_since_emit = 0;
                            emit_anim_fill(&app, &pool, "native");
                        }
                    }
                    Err(e) => {
                        log::warn!("[动画] seek 单线程填充失败（帧 {}）: {}", next_idx, e);
                        pool.set_degraded();
                        emit_anim_fill(&app, &pool, "native");
                        return;
                    }
                }
            }
            _ => {
                // EOF：首轮 → set total；全驻留 → 挂起；滑窗 → 重开从文件帧 0
                if !total_set {
                    pool.set_total(next_idx);
                    total_set = true;
                    emit_anim_fill(&app, &pool, "native");
                    if next_idx <= pool.capacity() as u64 {
                        pool.wait_closed();
                        return;
                    }
                }
                drop(dec);
                dec = match super::decode::jxl::AnimationDecoder::open_at(&path, 0) {
                    Ok(d) => d,
                    Err(_) => return,
                };
            }
        }
    }
}

/// 当前帧显示时长（池模式：在屏 guard 的 duration；resident_loop：缓存项）
fn current_frame_duration(anim: &AnimPlay) -> u32 {
    if let Some(cache) = anim.resident_loop.as_ref() {
        if !cache.is_empty() {
            let idx = anim.loop_idx % cache.len();
            return cache[idx].1;
        }
    }
    anim.held.last().map(|g| g.duration_ms).unwrap_or(33)
}

/// GPU 纹理 staging 读回（scRGB f16 RGBA，行距 = w*8）
unsafe fn readback_texture(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    w: u32,
    h: u32,
) -> Result<Vec<u8>, String> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
    };
    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut src_desc);
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: src_desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    device
        .CreateTexture2D(&desc, None, Some(&mut staging))
        .map_err(|e| format!("staging 创建失败: {e}"))?;
    let staging = staging.ok_or("staging None")?;
    ctx.CopyResource(&staging, tex);
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .map_err(|e| format!("staging Map 失败: {e}"))?;
    let row_pitch = w as usize * 8;
    let mut out = vec![0u8; row_pitch * h as usize];
    for y in 0..h as usize {
        let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
        let dst = y * row_pitch;
        std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(dst), row_pitch);
    }
    ctx.Unmap(&staging, 0);
    Ok(out)
}

impl HdrView {
    /// WM_CREATE 入口：GPU 池模式下初始化中途失败必须 close 池
    ///（唤醒/终止填充线程；审查问题 #4 的池版失败路径）
    unsafe fn create(hwnd: HWND, startup: Box<Startup>) -> Result<Self, String> {
        let pool_cleanup = startup.anim.as_ref().and_then(|a| a.pool.clone());
        let r = Self::create_inner(hwnd, startup);
        if r.is_err() {
            if let Some(p) = pool_cleanup {
                p.close();
            }
        }
        r
    }

    unsafe fn create_inner(hwnd: HWND, startup: Box<Startup>) -> Result<Self, String> {
        let Startup {
            src,
            name,
            monitors,
            app,
            embedded,
            anim,
        } = *startup;
        let (img_w, img_h) = (src.width.max(1), src.height.max(1));
        // 诊断采样：中心像素 scRGB f16（首帧日志打印，验证纹理数据非零；
        // GPU 池模式无 CPU 像素 → 全零占位）
        let cidx = ((img_h / 2) as usize * img_w as usize + (img_w / 2) as usize) * 8;
        let center_px: [u8; 8] = if cidx + 8 <= src.data.len() {
            src.data[cidx..cidx + 8].try_into().unwrap_or([0; 8])
        } else {
            [0; 8]
        };
        let pool_mode = anim.as_ref().map(|a| a.pool.is_some()).unwrap_or(false);

        // --- 设备：GPU 池模式共用 GpuEngine 单例（填充线程 compute 与本窗口
        //     draw 同 immediate context，引擎锁串行；渲染每帧全量重绑的前提）；
        //     静态图 / ring 路径保持独立设备 ---
        let (device, ctx) = if pool_mode {
            let eng = d3d11::engine()
                .as_ref()
                .map_err(|e| format!("GPU 引擎不可用: {e}"))?;
            let g = eng.0.lock().map_err(|e| format!("GPU 引擎锁: {e}"))?;
            let (d, c) = g.device_ctx();
            (d.clone(), c.clone())
        } else {
            create_device()?
        };

        // --- scRGB FP16 翻转交换链 ---
        let factory: IDXGIFactory2 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {}", e))?;
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let scd = DXGI_SWAP_CHAIN_DESC1 {
            Width: (rc.right - rc.left).max(1) as u32,
            Height: (rc.bottom - rc.top).max(1) as u32,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            Stereo: BOOL(0),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let sc1: IDXGISwapChain1 = factory
            .CreateSwapChainForHwnd(&device, hwnd, &scd, None, None::<&IDXGIOutput>)
            .map_err(|e| format!("CreateSwapChainForHwnd: {}", e))?;
        let sc3: IDXGISwapChain3 = sc1
            .cast()
            .map_err(|e| format!("IDXGISwapChain3（需 Win10 1703+）: {}", e))?;
        sc3.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)
            .map_err(|e| format!("SetColorSpace1(scRGB): {}", e))?;
        let swap: IDXGISwapChain = sc3.into();

        let rtv = make_rtv(&device, &swap)?;

        // --- 源纹理 ---
        // GPU 池模式：无独立源纹理——首帧（idx=0）已在打开流程 GPU 直写入池，
        // 此处 wait_frame(0) 确认就绪后 get 独占取出（在屏 guard 贯穿到
        // AnimPlay），SRV 直接取自池纹理；ring/静态图：独立纹理 + HdrSource
        // 直传（1.0 = 80 nits 绝对语义）
        let first_guard: Option<GpuFrame<PoolTex>> = if pool_mode {
            let pool = anim
                .as_ref()
                .and_then(|a| a.pool.clone())
                .ok_or("池模式缺池")?;
            if !pool.wait_frame(0) {
                return Err("池首帧未就绪（池已关闭）".into());
            }
            Some(pool.get(0).ok_or("池首帧取回失败")?)
        } else {
            None
        };
        let (tex, srv) = match &first_guard {
            Some(g) => (None, g.tex().srv.clone()),
            None => {
                let tex_desc = D3D11_TEXTURE2D_DESC {
                    Width: img_w,
                    Height: img_h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let init = D3D11_SUBRESOURCE_DATA {
                    pSysMem: src.data.as_ptr() as *const core::ffi::c_void,
                    SysMemPitch: img_w * 8,
                    SysMemSlicePitch: 0,
                };
                let mut tex = None;
                device
                    .CreateTexture2D(&tex_desc, Some(&init), Some(&mut tex))
                    .map_err(|e| format!("CreateTexture2D: {}", e))?;
                let tex: ID3D11Texture2D = tex.ok_or("纹理创建失败")?;
                let mut srv = None;
                device
                    .CreateShaderResourceView(&tex, None, Some(&mut srv))
                    .map_err(|e| format!("CreateShaderResourceView: {}", e))?;
                let srv: ID3D11ShaderResourceView = srv.ok_or("SRV 创建失败")?;
                (Some(tex), srv)
            }
        };

        // --- 采样器（线性 + 边界钳制）---
        let samp_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 0,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: -f32::MAX,
            MaxLOD: f32::MAX,
        };
        let mut sampler = None;
        device
            .CreateSamplerState(&samp_desc, Some(&mut sampler))
            .map_err(|e| format!("CreateSamplerState: {}", e))?;
        let sampler: ID3D11SamplerState = sampler.ok_or("采样器创建失败")?;

        // --- 着色器：优先启动预热缓存（零编译开销）；未预热时运行时编译兜底 ---
        let (vs_bytes, ps_bytes): (Vec<u8>, Vec<u8>) = match SHADER_CACHE.get() {
            Some((vs, ps)) => (vs.clone(), ps.clone()),
            None => {
                log::info!("[真HDR] 着色器缓存未命中（预热未完成？），运行时编译");
                let vs_blob = compile_shader(VS_HLSL, b"vs_4_0\0")?;
                let ps_blob = compile_shader(PS_HLSL, b"ps_4_0\0")?;
                (blob_bytes(&vs_blob).to_vec(), blob_bytes(&ps_blob).to_vec())
            }
        };
        let mut vs = None;
        device
            .CreateVertexShader(
                &vs_bytes,
                None::<&windows::Win32::Graphics::Direct3D11::ID3D11ClassLinkage>,
                Some(&mut vs),
            )
            .map_err(|e| format!("CreateVertexShader: {}", e))?;
        let vs: ID3D11VertexShader = vs.ok_or("VS 创建失败")?;
        let mut ps = None;
        device
            .CreatePixelShader(
                &ps_bytes,
                None::<&windows::Win32::Graphics::Direct3D11::ID3D11ClassLinkage>,
                Some(&mut ps),
            )
            .map_err(|e| format!("CreatePixelShader: {}", e))?;
        let ps: ID3D11PixelShader = ps.ok_or("PS 创建失败")?;

        // --- 输入布局 + 顶点缓冲 + 常量缓冲 ---
        let elems = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(b"POSITION\0".as_ptr()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(b"TEXCOORD\0".as_ptr()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 8,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let mut layout = None;
        device
            .CreateInputLayout(&elems, &vs_bytes, Some(&mut layout))
            .map_err(|e| format!("CreateInputLayout: {}", e))?;
        let layout: ID3D11InputLayout = layout.ok_or("输入布局创建失败")?;

        let quad: [Vtx; 4] = [
            Vtx {
                pos: [-1.0, 1.0],
                uv: [0.0, 0.0],
            }, // 左上
            Vtx {
                pos: [1.0, 1.0],
                uv: [1.0, 0.0],
            }, // 右上
            Vtx {
                pos: [-1.0, -1.0],
                uv: [0.0, 1.0],
            }, // 左下
            Vtx {
                pos: [1.0, -1.0],
                uv: [1.0, 1.0],
            }, // 右下
        ];
        let vb_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<[Vtx; 4]>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let vb_init = D3D11_SUBRESOURCE_DATA {
            pSysMem: quad.as_ptr() as *const core::ffi::c_void,
            SysMemPitch: 0,
            SysMemSlicePitch: 0,
        };
        let mut vb = None;
        device
            .CreateBuffer(&vb_desc, Some(&vb_init), Some(&mut vb))
            .map_err(|e| format!("CreateBuffer(VB): {}", e))?;
        let vb: ID3D11Buffer = vb.ok_or("顶点缓冲创建失败")?;

        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<QuadParams>() as u32, // 16 字节
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut cbuf = None;
        device
            .CreateBuffer(&cb_desc, None, Some(&mut cbuf))
            .map_err(|e| format!("CreateBuffer(CB): {}", e))?;
        let cbuf: ID3D11Buffer = cbuf.ok_or("常量缓冲创建失败")?;

        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        // 动画模式：SetTimer 驱动逐帧翻页（10ms tick，帧内累计时长判断推进）
        if anim.is_some() {
            SetTimer(hwnd, ANIM_TIMER_ID, ANIM_TIMER_MS, None);
        }
        let mut v = HdrView {
            device,
            ctx,
            swap,
            rtv: Some(rtv),
            srv,
            sampler,
            vs,
            ps,
            layout,
            vb,
            cbuf,
            img_w,
            img_h,
            cw: (rc.right - rc.left).max(0) as u32,
            ch: (rc.bottom - rc.top).max(0) as u32,
            fit: true,
            zoom: 1.0,
            cx: 0.0,
            cy: 0.0,
            drag: None,
            cursor_arrow: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            cursor_size: LoadCursorW(None, IDC_SIZEALL).unwrap_or_default(),
            name,
            monitors,
            cur_hdr: true,
            cur_max_lum: 0.0,
            app,
            embedded,
            hook: None, // run_window 创建后注入（SetWinEventHook 句柄）
            center_px,
            bg_white: {
                let nits = get_sdr_white_level_nits();
                let v = (nits / 80.0).clamp(1.0, 12.5); // 下限 80nits；上限 1000nits 防虚标
                log::info!(
                    "[真HDR] 白底: 系统SDR白={:.0}nits → scRGB {:.2}（与图像白/SDR模式白同亮）",
                    nits,
                    v
                );
                v
            },
            tex,
            anim: anim.map(|a| AnimPlay {
                ring: a.ring,
                pool: a.pool,
                // 首帧 guard（池模式首帧 0 在屏即首个持有；ring 恒空）
                held: match first_guard {
                    Some(g) => vec![g],
                    None => Vec::new(),
                },
                // 首帧 0 已在屏（guard 持有）→ 下一帧序号 1；ring 路径恒 0（不消费）
                next_frame_idx: if pool_mode { 1 } else { 0 },
                resident_loop: None,
                loop_idx: 0,
                sliding: false,
                sacrificed: Vec::new(),
                // 首帧立即显示后，下一帧到期 = 首帧时长后（绝对时钟起点）
                next_due: Instant::now()
                    + std::time::Duration::from_millis(a.first_duration_ms.max(1) as u64),
                backend: a.backend,
                path: a.path,
                paused: false,
                cur_frame: 0,
                abs_total: None,
                session_base: 0,
            }),
            fullscreen: false,
            fs_saved: None,
        };
        v.apply_fit();
        v.refresh_display_state(hwnd);
        Ok(v)
    }

    /// 自适应缩放基准（图像完整显示在客户区的最大比例）
    fn fit_scale(&self) -> f32 {
        if self.cw > 0 && self.ch > 0 {
            (self.cw as f32 / self.img_w as f32).min(self.ch as f32 / self.img_h as f32)
        } else {
            1.0
        }
    }

    fn cur_scale(&self) -> f32 {
        if self.fit {
            self.fit_scale()
        } else {
            self.zoom
        }
    }

    fn apply_fit(&mut self) {
        self.fit = true;
        self.zoom = 1.0;
        self.cx = self.cw as f32 / 2.0;
        self.cy = self.ch as f32 / 2.0;
    }

    fn set_100(&mut self) {
        self.fit = false;
        self.zoom = 1.0;
        self.cx = self.cw as f32 / 2.0;
        self.cy = self.ch as f32 / 2.0;
    }

    /// 双击切换：当前 ≈1:1 → 自适应；否则 → 1:1
    fn toggle_scale(&mut self) {
        if !self.fit && (self.zoom - 1.0).abs() < 0.02 {
            self.apply_fit();
        } else {
            self.set_100();
        }
    }

    /// 以光标 (mx,my) 为锚缩放（光标下的图像点保持不动）
    fn zoom_at(&mut self, mx: f32, my: f32, factor: f32) {
        let old = self.cur_scale();
        let fs = self.fit_scale();
        let new = (old * factor).clamp(fs * 0.02, 64.0);
        if old > 0.0 {
            self.fit = false;
            self.zoom = new;
            self.cx = mx - (mx - self.cx) * (new / old);
            self.cy = my - (my - self.cy) * (new / old);
        }
    }

    /// 窗口尺寸变化：重建 RTV + 自适应时重算布局
    unsafe fn on_size(&mut self, hwnd: HWND, w: u32, h: u32) {
        self.cw = w;
        self.ch = h;
        if w == 0 || h == 0 {
            return;
        }
        self.rtv = None; // ResizeBuffers 前必须释放
        if let Err(e) =
            self.swap
                .ResizeBuffers(0, 0, 0, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
        {
            log::error!("[真HDR] ResizeBuffers: {}", e);
        }
        match make_rtv(&self.device, &self.swap) {
            Ok(rtv) => self.rtv = Some(rtv),
            Err(e) => log::error!("[真HDR] 重建 RTV: {}", e),
        }
        if self.fit {
            self.apply_fit();
        }
        let _ = InvalidateRect(hwnd, None, false);
    }

    /// 窗口所在显示器 HDR 状态变化 → 更新标题（嵌入模式无标题栏，仅记录状态）
    fn refresh_display_state(&mut self, hwnd: HWND) {
        if self.embedded {
            return; // 子窗口无标题栏；HDR 状态由前端状态栏展示
        }
        let hmon: HMONITOR = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        let m = self.monitors.iter().find(|x| x.hmonitor == hmon.0 as isize);
        let (hdr, max_lum) = match m {
            Some(x) => (x.is_hdr(), x.max_full_frame_luminance),
            None => (false, 0.0),
        };
        if hdr != self.cur_hdr || (max_lum - self.cur_max_lum).abs() > 0.5 {
            self.cur_hdr = hdr;
            self.cur_max_lum = max_lum;
            let title = if hdr {
                format!(
                    "真 HDR 查看 · {} · {}×{} · 峰值 {:.0} nits · 滚轮缩放 · 拖动平移 · 双击 1:1 · Esc 关闭",
                    self.name, self.img_w, self.img_h, max_lum
                )
            } else {
                format!(
                    "真 HDR 查看 · {} · {}×{} ·（当前显示器 HDR 未开启，>SDR 白的部分将被裁剪）",
                    self.name, self.img_w, self.img_h
                )
            };
            unsafe {
                let _ = SetWindowTextW(hwnd, PCWSTR(to_wide(&title).as_ptr()));
            }
        }
    }

    /// 渲染一帧：Clear → 上传矩阵 → 四边形直通 scRGB → Present
    ///
    /// GPU 池模式（anim.pool Some）：绘制命令在 GpuEngine 引擎锁内执行（与
    /// 填充线程 compute 共用 immediate context，Mutex 串行——draw_frame 每帧
    /// 全量重绑管线状态，不残留/不假设上一命令的状态），Present **锁外**
    /// （vsync 等待不阻塞填充线程）。静态图 / ring 模式：独立设备直接渲染。
    unsafe fn render(&mut self) {
        if self.cw == 0 || self.ch == 0 {
            return;
        }
        let pool_mode = self
            .anim
            .as_ref()
            .map(|a| a.pool.is_some())
            .unwrap_or(false);
        let hr = if pool_mode {
            let eng = match d3d11::engine() {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("[真HDR] 渲染跳帧（引擎不可用）: {}", e);
                    return;
                }
            };
            let Ok(_g) = eng.0.lock() else {
                return; // 引擎锁毒化（不应发生）：跳帧
            };
            self.draw_frame();
            drop(_g); // Present 锁外
            self.swap.Present(1, DXGI_PRESENT(0))
        } else {
            self.draw_frame();
            self.swap.Present(1, DXGI_PRESENT(0))
        };
        if hr.is_err() {
            log::warn!("[真HDR] Present: {:?}", hr);
        }
    }

    /// 绘制命令（不含 Present）：每帧全量重绑（cbuf Map → Clear → OM/RS/IA/
    /// VS/PS/SRV/Sampler → Draw）——GPU 池模式下 compute（填充线程）与 draw
    /// 共享 context 的前提：不依赖上一命令遗留的任何绑定状态。
    unsafe fn draw_frame(&mut self) {
        let Some(rtv) = self.rtv.clone() else { return };
        let scale = self.cur_scale();
        let dw = self.img_w as f32 * scale;
        let dh = self.img_h as f32 * scale;
        let params = QuadParams {
            half: [dw / self.cw as f32, dh / self.ch as f32],
            center: [
                2.0 * self.cx / self.cw as f32 - 1.0,
                1.0 - 2.0 * self.cy / self.ch as f32,
            ],
        };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if self
            .ctx
            .Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
            .is_ok()
        {
            *(mapped.pData as *mut QuadParams) = params;
            self.ctx.Unmap(&self.cbuf, 0);
        }

        // 背景清屏：全屏 = 纯黑（0 nits，影院黑布 letterbox）；窗口 = 系统
        // SDR 白电平（bg_white，create 时读取）——与图像内容的白（截图屏幕白
        // 直通）和 SDR 模式 WebView2 白底严格同亮，纯白观感。
        // 旧版 Clear(1,1,1) 在 SDR 白滑块偏低的面板上比图像白暗 → 发灰。
        let w = if self.fullscreen { 0.0 } else { self.bg_white };
        self.ctx.ClearRenderTargetView(&rtv, &[w, w, w, 1.0]);
        self.ctx.OMSetRenderTargets(
            Some(&[Some(rtv)]),
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
        );
        let vp = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.cw as f32,
            Height: self.ch as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        self.ctx.RSSetViewports(Some(&[vp]));
        self.ctx.IASetInputLayout(&self.layout);
        self.ctx
            .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
        let vb = self.vb.clone();
        let stride = std::mem::size_of::<Vtx>() as u32;
        let offset = 0u32;
        self.ctx
            .IASetVertexBuffers(0, 1, Some(&Some(vb)), Some(&stride), Some(&offset));
        self.ctx.VSSetShader(&self.vs, None);
        // 关键：绑定常量缓冲到 VS 槽位 b0（cbuffer Params : register(b0)）。
        // 此前缺失该绑定 → 驱动读到全零 → halfSize=(0,0) → 四顶点全部塌缩到
        // NDC 原点 → 退化三角形不产生像素 → 画面只剩 Clear 的黑色（黑屏根因）。
        self.ctx
            .VSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
        self.ctx.PSSetShader(&self.ps, None);
        self.ctx
            .PSSetShaderResources(0, Some(&[Some(self.srv.clone())]));
        self.ctx
            .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        self.ctx.Draw(4, 0);
        // 解绑 PS SRV（槽 0）：GPU 池模式下该槽纹理 draw 完即可能被填充线程
        // UpdateSubresource 复用（在屏 guard 归还 → 槽位重回 free，native 后端
        // 直写该纹理）——UpdateSubresource 目标若仍绑定在 context 上会走驱动
        // "暂存拷贝 + 延迟更新"路径。本帧 Draw 已提交，此处解绑只影响后续命令，
        // 保证填充线程拿到槽位时该纹理必然不在绑。
        self.ctx.PSSetShaderResources(0, Some(&[None]));

        // 首帧诊断：确认渲染链路（参数/纹理采样值/绘制；Present 结果由 render 打印）
        static FIRST_RENDER: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(true);
        if FIRST_RENDER.swap(false, std::sync::atomic::Ordering::Relaxed) {
            let f16v = |o: usize| u16::from_le_bytes([self.center_px[o], self.center_px[o + 1]]);
            log::info!(
                "[真HDR] 首帧: 画布{}x{} 图{}x{} scale={:.2} half=({:.3},{:.3}) center=({:.3},{:.3}) 中心像素scRGB=({:.3},{:.3},{:.3})",
                self.cw, self.ch, self.img_w, self.img_h, scale,
                params.half[0], params.half[1], params.center[0], params.center[1],
                f16_to_f32(f16v(0)),
                f16_to_f32(f16v(2)),
                f16_to_f32(f16v(4)),
            );
        }
    }
}

/// f16 → f32（诊断采样用）
fn f16_to_f32(v: u16) -> f32 {
    let sign = ((v >> 15) & 1) as u32;
    let exp = ((v >> 10) & 0x1F) as u32;
    let mant = (v & 0x3FF) as u32;
    if exp == 0 {
        return if mant == 0 {
            f32::from_bits(sign << 31)
        } else {
            // 次正规
            let e = -14i32 - (mant.leading_zeros() as i32 - 22);
            let m = (mant << (mant.leading_zeros() + 1)) & 0x3FF;
            f32::from_bits((sign << 31) | (((127 - 15 + e + 1) as u32) << 23) | (m << 13))
        };
    }
    if exp == 0x1F {
        return f32::NAN;
    }
    f32::from_bits((sign << 31) | ((exp + 127 - 15) << 23) | (mant << 13))
}

/// 创建 D3D11 设备（硬件优先，失败回退 WARP 软渲染）
unsafe fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let fl = [D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    let mut ctx = None;
    let ok = D3D11CreateDevice(
        None::<&IDXGIAdapter>,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&fl),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut ctx),
    )
    .is_ok();
    if !ok {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&fl),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .map_err(|e| format!("D3D11CreateDevice(WARP): {}", e))?;
    }
    match (device, ctx) {
        (Some(d), Some(c)) => Ok((d, c)),
        _ => Err("D3D11 设备创建失败".to_string()),
    }
}

/// 从后台缓冲创建 RTV
unsafe fn make_rtv(
    device: &ID3D11Device,
    swap: &IDXGISwapChain,
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView, String> {
    let buf: ID3D11Texture2D = swap.GetBuffer(0).map_err(|e| format!("GetBuffer: {}", e))?;
    let mut rtv = None;
    device
        .CreateRenderTargetView(&buf, None, Some(&mut rtv))
        .map_err(|e| format!("CreateRenderTargetView: {}", e))?;
    rtv.ok_or_else(|| "RTV 创建失败".to_string())
}

/// 运行时编译着色器（d3dcompiler_47.dll，Win10+ 系统自带）
unsafe fn compile_shader(src: &[u8], target: &[u8]) -> Result<ID3DBlob, String> {
    let mut code = None;
    let mut errs = None;
    let hr = D3DCompile(
        src.as_ptr() as *const core::ffi::c_void,
        src.len(),
        PCSTR(b"hdr_view.hlsl\0".as_ptr()),
        None,
        None::<&ID3DInclude>,
        PCSTR(b"main\0".as_ptr()),
        PCSTR(target.as_ptr()),
        0,
        0,
        &mut code,
        Some(&mut errs),
    );
    if hr.is_err() {
        let msg = errs
            .map(|b| {
                let p = b.GetBufferPointer() as *const u8;
                let n = b.GetBufferSize();
                let s = std::slice::from_raw_parts(p, n);
                String::from_utf8_lossy(s).into_owned()
            })
            .unwrap_or_default();
        return Err(format!("D3DCompile: {:?} {}", hr, msg));
    }
    code.ok_or_else(|| "编译无产物".to_string())
}

unsafe fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    let p = b.GetBufferPointer() as *const u8;
    if p.is_null() {
        return &[];
    }
    std::slice::from_raw_parts(p, b.GetBufferSize())
}

/// 选取锚定显示器（viewer 窗口所在；无则主屏）
fn pick_monitor(anchor: Option<HWND>) -> MonitorInfo {
    let monitors = enumerate_monitors().unwrap_or_default();
    let hmon = match anchor {
        Some(hwnd) => unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) },
        None => unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) },
    };
    monitors
        .iter()
        .find(|m| m.hmonitor == hmon.0 as isize)
        .cloned()
        .or_else(|| monitors.first().cloned())
        .unwrap_or(MonitorInfo {
            adapter_index: 0,
            output_index: 0,
            adapter_name: String::new(),
            device_name: String::from("\\\\.\\DISPLAY1"),
            width: 1920,
            height: 1080,
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
            bits_per_color: 8,
            hdr_mode: crate::capture::monitor::HdrMode::Sdr,
            sdr_white_level_nits: 80.0,
            max_full_frame_luminance: 80.0,
            max_luminance: 80.0,
            attached_to_desktop: true,
            hmonitor: 0,
            gamut_r: (0.0, 0.0),
            gamut_g: (0.0, 0.0),
            gamut_b: (0.0, 0.0),
            gamut_white: (0.0, 0.0),
            gamut_valid: false,
            edid: None,
        })
}
