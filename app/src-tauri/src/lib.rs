//! jietu-hdr Tauri 后端库
//!
//! 将 HDR 核心模块（capture / color / encode / config）适配为 Tauri 命令，
//! 供前端 Vue 应用通过 `invoke` 调用。

mod assoc;
pub mod capture;
pub mod color;
pub mod config;
pub mod encode;
mod memory;
mod record;
mod singleinstance;
pub mod upscale;
pub mod video;
pub mod viewer;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use capture::{to_sdr, DesktopCapturer, HdrToSdrParams};
use color::TonemapOperator;
use config::{Config, Hotkey, MainWindowState};
use encode::OutputFormat;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    webview::WebviewWindowBuilder,
    Emitter, Manager,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{
    Builder as ShortcutBuilder, Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
};

/// 运行时热键状态（用于在 global-shortcut 回调中识别触发的热键）
#[derive(Debug, Clone, Default)]
struct HotkeyState {
    region: Option<Hotkey>,
    fullscreen: Option<Hotkey>,
    silent: Option<Hotkey>,
    /// 清理内存热键（完整复刻 memreduct SOURCE_HOTKEY）
    clean: Option<Hotkey>,
    /// 开始录制热键（游戏模式直启；None = 未启用）
    record_start: Option<Hotkey>,
    /// 停止录制热键（配置值；录制期间由沉浸式临时注册）
    record_stop: Option<Hotkey>,
    /// 开始视频录制热键（游戏模式直启 MKV + OSD；None = 未启用）
    video_start: Option<Hotkey>,
    /// 停止视频录制热键（配置值；视频录制期间由沉浸式临时注册）
    video_stop: Option<Hotkey>,
}

/// 主窗口几何状态（内存缓存 + 脏标记；后台线程防抖落盘到 config.toml）
static MAIN_WIN_STATE: Mutex<Option<MainWindowState>> = Mutex::new(None);
static MAIN_WIN_DIRTY: AtomicBool = AtomicBool::new(false);
/// 几何录制开关：主面板视图时 true；切到设置中心期间 false（避免把设置页尺寸记成主面板尺寸）
static MAIN_WIN_RECORDING: AtomicBool = AtomicBool::new(true);
/// region-select 覆盖层心跳（前端渲染循环每秒上报；停止 = 渲染进程崩溃/卡死，看门狗强制收起）
static REGION_HEARTBEAT: AtomicU64 = AtomicU64::new(0);
/// 看图窗口是否有标签打开（前端 tabs 变化时上报；决定 X 关标签还是关窗口）
static VIEWER_TABS_OPEN: AtomicBool = AtomicBool::new(false);

/// 启动即视频播放（文件关联双击视频）：抑制主窗口显示——播放器窗口是唯一界面，
/// 播放器关闭后无界面应用直接退出（V17；此前主面板总是跟着弹出）
static STARTUP_VIDEO_LAUNCH: AtomicBool = AtomicBool::new(false);

/// 前端上报看图窗口标签态（有标签时点 X = 关当前标签回相册页，无标签 = 隐藏窗口）
#[tauri::command]
fn set_viewer_tabs_open(open: bool) {
    VIEWER_TABS_OPEN.store(open, Ordering::Relaxed);
}

/// 前端主窗口就绪信号：显示主窗口。
/// 配合 tauri.conf.json 的 visible:false——窗口等 WebView 渲染出启动页（load 事件）
/// 才出现，消除启动期"透明内容 + 边框线"的空窗阶段。
#[tauri::command]
fn frontend_main_ready(app: tauri::AppHandle) {
    // 启动即视频播放：主窗口保持隐藏（播放器窗口即全部界面）
    if STARTUP_VIDEO_LAUNCH.load(Ordering::Relaxed) {
        log::info!("[启动] 启动即播放模式：主窗口保持隐藏");
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        log::info!("[启动] 前端就绪，主窗口显示");
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 覆盖层前端心跳上报（渲染循环活着才会发；看门狗据此判断是否卡死）
#[tauri::command]
fn region_heartbeat() {
    REGION_HEARTBEAT.store(now_secs(), Ordering::Relaxed);
}

/// 收起残留的 region-select 覆盖层（托盘唤起主窗口等兜底路径调用；
/// 覆盖层全屏置顶吞输入，渲染进程崩溃时 Esc 失效会锁死系统键盘）
fn hide_region_select_if_visible(app: &tauri::AppHandle) {
    if let Some(overlay) = app.get_webview_window("region-select") {
        if overlay.is_visible().unwrap_or(false) {
            let _ = overlay.hide();
            log::info!("已收起残留的 region-select 覆盖层");
        }
    }
}

/// 截图流程中主窗口的隐藏前状态
/// - Some(true)  = 面板发起（前端按钮 invoke）：截图完成后还原窗口并聚焦
/// - Some(false) = 热键发起：截图完成后保持最小化，不抢用户当前焦点
/// - None        = 本流程未处理主窗口（置顶保持显示 / 已最小化 / 已隐藏到托盘 / 配置不隐藏）
static CAPTURE_MAIN_STATE: Mutex<Option<bool>> = Mutex::new(None);
/// 显示窗口但不激活（不抢用户当前工作焦点）：ShowWindow(SW_SHOWNOACTIVATE)
fn show_window_no_activate(hwnd_raw: isize) {
    extern "system" {
        fn ShowWindow(hwnd: *mut std::ffi::c_void, ncmdshow: i32) -> i32;
    }
    const SW_SHOWNOACTIVATE: i32 = 4;
    unsafe {
        ShowWindow(hwnd_raw as *mut std::ffi::c_void, SW_SHOWNOACTIVATE);
    }
}

/// 显示窗口为最小化但不激活：ShowWindow(SW_SHOWMINNOACTIVE)
///
/// 已弃用（保留函数避免其他引用处连锁改动）：hide() 后 SW_SHOWMINNOACTIVE
/// 会造成"visible+iconic"的状态污染——任务栏按钮映射错乱，点击无反应。
#[allow(dead_code)]
fn show_window_minimized_no_activate(hwnd_raw: isize) {
    extern "system" {
        fn ShowWindow(hwnd: *mut std::ffi::c_void, ncmdshow: i32) -> i32;
    }
    const SW_SHOWMINNOACTIVE: i32 = 7;
    unsafe {
        ShowWindow(hwnd_raw as *mut std::ffi::c_void, SW_SHOWMINNOACTIVE);
    }
}

/// 窗口最小化（不激活）：ShowWindow(SW_MINIMIZE)
///
/// 系统原生最小化：任务栏按钮由 Windows 自身维护映射，点击可正常还原。
fn minimize_window_no_activate(hwnd_raw: isize) {
    extern "system" {
        fn ShowWindow(hwnd: *mut std::ffi::c_void, ncmdshow: i32) -> i32;
    }
    const SW_MINIMIZE: i32 = 6;
    unsafe {
        ShowWindow(hwnd_raw as *mut std::ffi::c_void, SW_MINIMIZE);
    }
}

/// 截图流程开始：按状态处理主窗口
/// - 已隐藏到托盘：跳过（隐藏窗口不入镜；且对隐藏窗口调 SW_MINIMIZE
///   会造成 "visible+iconic" 状态污染——任务栏出现按钮但点击无响应）
/// - 置顶（钉住）：保持显示（不入镜由捕获时临时排除处理）
/// - 已最小化：无需处理（最小化窗口不在屏幕画面中）
/// - 可见 + hide_on_capture（默认开）：最小化（避免入镜，任务栏可点回）
/// 记录发起来源（from_panel）：恢复时决定是否还原聚焦
fn hide_main_for_capture(app: &tauri::AppHandle, flow: &str, from_panel: bool) {
    let Some(main) = app.get_webview_window("main") else {
        *CAPTURE_MAIN_STATE.lock().unwrap() = None;
        return;
    };
    // 隐藏到托盘（点 X 关闭）：不会入镜，且最小化会造成任务栏状态污染
    if !main.is_visible().unwrap_or(true) {
        log::info!("{}：主窗口已隐藏到托盘，无需处理", flow);
        *CAPTURE_MAIN_STATE.lock().unwrap() = None;
    } else if main.is_always_on_top().unwrap_or(false) {
        log::info!("{}：主窗口置顶，截图期间保持显示", flow);
        *CAPTURE_MAIN_STATE.lock().unwrap() = None;
    } else if main.is_minimized().unwrap_or(false) {
        log::info!("{}：主窗口已最小化，无需处理（完成后保持最小化）", flow);
        *CAPTURE_MAIN_STATE.lock().unwrap() = None;
    } else if !Config::load().hide_main_on_capture {
        log::info!("{}：截图不隐藏主面板（配置关闭）", flow);
        *CAPTURE_MAIN_STATE.lock().unwrap() = None;
    } else {
        // 直接最小化（原生 SW_MINIMIZE）：任务栏映射由系统维护，
        // 点任务栏图标可正常还原；hide() 会造成状态污染（点任务栏无反应）
        if let Ok(hwnd) = main.hwnd() {
            minimize_window_no_activate(hwnd.0 as isize);
            log::info!(
                "{}：主窗口已最小化（{}发起）",
                flow,
                if from_panel { "面板" } else { "热键" }
            );
        } else {
            let _ = main.hide();
            log::warn!("{}：取主窗口句柄失败，退回 hide()", flow);
        }
        *CAPTURE_MAIN_STATE.lock().unwrap() = Some(from_panel);
    }
}

/// 截图流程结束：按记录状态恢复主窗口
/// - 未处理过（置顶/已最小化/已隐藏/配置不隐藏）：不动，保持原状态
/// - 面板发起（Some(true)）：还原窗口并聚焦（用户从面板发起的操作）
/// - 热键发起（Some(false)）：保持最小化，不抢用户当前焦点，
///   任务栏图标点一下即可回到截图前的页面（如设置画质页）
fn restore_main_after_capture(app: &tauri::AppHandle, flow: &str) {
    let st = CAPTURE_MAIN_STATE.lock().unwrap().take();
    let Some(from_panel) = st else {
        return;
    };
    if !from_panel {
        // 已是最小化态（SW_MINIMIZE），无需再动：不弹窗遮挡、不抢当前焦点
        if let Some(main) = app.get_webview_window("main") {
            if main.is_minimized().unwrap_or(false) {
                log::info!("{}：主窗口保持最小化（热键发起，不抢焦点）", flow);
            }
        }
        return;
    }
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.unminimize();
        let _ = main.set_focus();
        log::info!("{}：主窗口已恢复并聚焦（面板发起）", flow);
    }
}

/// 记录主窗口当前几何状态（Resized/Moved 事件调用；只更新内存，由落盘线程写盘）
fn record_main_window_state(window: &tauri::Window) {
    if !MAIN_WIN_RECORDING.load(Ordering::Relaxed) {
        return;
    }
    // 最小化状态不记录：-32000 是 Windows 最小化窗口的标志位置，
    // 恢复后窗口会跑到屏幕外（表现为主面板"打不开"）
    if window.is_minimized().unwrap_or(false) {
        return;
    }
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.inner_size()) else {
        return;
    };
    if pos.x <= -30000 || pos.y <= -30000 {
        return;
    }
    *MAIN_WIN_STATE.lock().unwrap() = Some(MainWindowState {
        x: pos.x,
        y: pos.y,
        width: size.width,
        height: size.height,
    });
    MAIN_WIN_DIRTY.store(true, Ordering::Relaxed);
}

/// 立即落盘主窗口几何状态（托盘退出 / 关闭主窗口时调用，避免防抖窗口内丢失）
fn flush_main_window_state() {
    if MAIN_WIN_DIRTY.swap(false, Ordering::Relaxed) {
        if let Some(ws) = *MAIN_WIN_STATE.lock().unwrap() {
            let mut cfg = Config::load();
            cfg.main_window = Some(ws);
            if let Err(e) = cfg.save() {
                log::warn!("主窗口状态保存失败: {:#}", e);
            }
        }
    }
}

/// 将 `Hotkey.modifiers` 位域转换为 tauri-plugin-global-shortcut 的 `Modifiers`
fn hk_mods_to_tauri(mods: u32) -> Modifiers {
    let mut m = Modifiers::empty();
    if mods & Hotkey::MOD_ALT != 0 {
        m |= Modifiers::ALT;
    }
    if mods & Hotkey::MOD_CONTROL != 0 {
        m |= Modifiers::CONTROL;
    }
    if mods & Hotkey::MOD_SHIFT != 0 {
        m |= Modifiers::SHIFT;
    }
    if mods & Hotkey::MOD_WIN != 0 {
        m |= Modifiers::SUPER;
    }
    m
}

/// 将虚拟键码 `vk` 映射为 tauri-plugin-global-shortcut 的 `Code`
///
/// 仅覆盖常用截图键（A-Z、0-9、PrintScreen、F1-F12）。
fn vk_to_code(vk: u32) -> Option<Code> {
    // A-Z
    if (0x41..=0x5A).contains(&vk) {
        let idx = (vk - 0x41) as usize;
        let codes = [
            Code::KeyA,
            Code::KeyB,
            Code::KeyC,
            Code::KeyD,
            Code::KeyE,
            Code::KeyF,
            Code::KeyG,
            Code::KeyH,
            Code::KeyI,
            Code::KeyJ,
            Code::KeyK,
            Code::KeyL,
            Code::KeyM,
            Code::KeyN,
            Code::KeyO,
            Code::KeyP,
            Code::KeyQ,
            Code::KeyR,
            Code::KeyS,
            Code::KeyT,
            Code::KeyU,
            Code::KeyV,
            Code::KeyW,
            Code::KeyX,
            Code::KeyY,
            Code::KeyZ,
        ];
        return Some(codes[idx]);
    }
    // 0-9 (VK_0=0x30 ... VK_9=0x39)
    if (0x30..=0x39).contains(&vk) {
        let idx = (vk - 0x30) as usize;
        let codes = [
            Code::Digit0,
            Code::Digit1,
            Code::Digit2,
            Code::Digit3,
            Code::Digit4,
            Code::Digit5,
            Code::Digit6,
            Code::Digit7,
            Code::Digit8,
            Code::Digit9,
        ];
        return Some(codes[idx]);
    }
    // F1-F12 (VK_F1=0x70 ... VK_F12=0x7B)
    if (0x70..=0x7B).contains(&vk) {
        let idx = (vk - 0x70) as usize;
        let codes = [
            Code::F1,
            Code::F2,
            Code::F3,
            Code::F4,
            Code::F5,
            Code::F6,
            Code::F7,
            Code::F8,
            Code::F9,
            Code::F10,
            Code::F11,
            Code::F12,
        ];
        return Some(codes[idx]);
    }
    match vk {
        0x2C => Some(Code::PrintScreen),
        0x20 => Some(Code::Space),
        0x0D => Some(Code::Enter),
        0x1B => Some(Code::Escape),
        0x08 => Some(Code::Backspace),
        0x09 => Some(Code::Tab),
        0x2D => Some(Code::Insert),
        0x2E => Some(Code::Delete),
        0x24 => Some(Code::Home),
        0x23 => Some(Code::End),
        0x21 => Some(Code::PageUp),
        0x22 => Some(Code::PageDown),
        0x25 => Some(Code::ArrowLeft),
        0x26 => Some(Code::ArrowUp),
        0x27 => Some(Code::ArrowRight),
        0x28 => Some(Code::ArrowDown),
        _ => None,
    }
}

/// 将内部 `Hotkey` 转换为 tauri 的 `Shortcut`
fn hotkey_to_shortcut(hk: &Hotkey) -> Option<Shortcut> {
    let key = vk_to_code(hk.vk)?;
    Some(Shortcut::new(Some(hk_mods_to_tauri(hk.modifiers)), key))
}

/// 热键显示名（"Alt+F10" 样式；无法映射返回 None）
pub(crate) fn hotkey_display_name(hk: &Hotkey) -> Option<String> {
    hotkey_to_shortcut(hk).map(|sc| {
        let s = sc.into_string();
        // 统一首字母大写风格（tauri 输出 "alt+F10"）
        match s.split_once('+') {
            Some((m, k)) => {
                let mut parts: Vec<String> = m.split('+').map(|p| capitalize(p)).collect();
                parts.push(k.to_string());
                parts.join("+")
            }
            None => s,
        }
    })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// 录制停止热键桥接（record/commands.rs 调用；注册失败仅告警不阻断）
pub(crate) fn try_register_stop_hotkey(app: &tauri::AppHandle, hk: Hotkey) -> bool {
    match hotkey_to_shortcut(&hk) {
        Some(sc) => {
            let desc = sc.into_string();
            match app.global_shortcut().register(sc) {
                Ok(()) => {
                    log::info!("[record] 停止热键已注册: {}", desc);
                    true
                }
                Err(e) => {
                    log::warn!("[record] 停止热键注册失败 ({}): {}", desc, e);
                    false
                }
            }
        }
        None => false,
    }
}

pub(crate) fn try_unregister_stop_hotkey(app: &tauri::AppHandle, hk: Hotkey) {
    if let Some(sc) = hotkey_to_shortcut(&hk) {
        if let Err(e) = app.global_shortcut().unregister(sc) {
            log::warn!("[record] 停止热键注销失败: {}", e);
        }
    }
}

/// 获取当前配置
#[tauri::command]
fn get_config() -> Config {
    Config::load()
}

/// 保存配置
#[tauri::command]
fn save_config(app: tauri::AppHandle, config: Config) -> Result<(), String> {
    // 保存配置
    config.save().map_err(|e| format!("{:#}", e))?;
    // 配置变更后同步注册热键
    sync_hotkeys(app, &config).map_err(|e| format!("{:#}", e))?;
    Ok(())
}

// ==================== 系统集成：文件关联 + 右键菜单（assoc.rs） ====================

/// 查询文件关联注册状态（以注册表实际状态为准）
#[tauri::command]
fn is_file_assoc() -> bool {
    assoc::is_registered()
}

/// 开/关文件关联（注册表写入 + 配置持久化，幂等）
#[tauri::command]
fn set_file_assoc(enabled: bool) -> Result<(), String> {
    if enabled {
        assoc::register()?;
    } else {
        assoc::unregister()?;
    }
    // 持久化到 config.toml（卸载/迁移时可据此清理或恢复）
    let mut cfg = Config::load();
    cfg.file_assoc = enabled;
    cfg.save().map_err(|e| format!("保存配置失败: {:#}", e))?;
    Ok(())
}

/// 设为默认看图应用：调起系统「设置关联」确认界面（应用专属页）
///
/// Windows 8+ 禁止程序静默改 UserChoice（系统哈希保护），必须经用户确认；
/// 调用为模态阻塞（至用户关闭界面）→ spawn_blocking 执行，前端 await 返回后刷新回显。
#[tauri::command]
async fn open_default_apps() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(assoc::launch_default_apps_ui)
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 查询默认应用明细：逐扩展名返回归属状态（ours/owner/ProgID）
///
/// UserChoice 只读检测（Win10/11 哈希保护写入，程序不可静默改默认）。
#[tauri::command]
fn get_assoc_detail() -> Vec<assoc::AssocExtStatus> {
    assoc::assoc_detail()
}

// ==================== 启动参数路由（右键菜单 / 双击图片入口） ====================

/// 启动参数解析结果
enum LaunchAction {
    None,
    /// 直接路径参数（双击图片 / 「打开方式」）→ 看图窗口打开
    OpenImage(String),
    /// 视频文件路径（「打开方式」指向本程序）→ 原生播放器窗口
    OpenVideo(String),
    /// --library <目录>（右键文件夹）→ 相册页挂载并浏览
    Library(String),
    /// --vault-add <路径...>（右键「添加到隐私相册」）→ 加密移入
    VaultAdd(Vec<String>),
}

/// 启动参数路由支持的视频扩展名（「打开方式」/拖放播放）
const VIDEO_LAUNCH_EXTS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "flv", "ts", "m4v"];

/// 解析命令行参数（-clean 已在启动早期单独处理，不会到这）
/// （pub(crate) 供 singleinstance 队列转发复用）
fn parse_launch_args(args: &[String]) -> LaunchAction {
    let mut i = 1; // 跳过 argv[0]
    while i < args.len() {
        let a = &args[i];
        if a == "--vault-add" {
            let mut paths = Vec::new();
            i += 1;
            while i < args.len() && !args[i].starts_with("--") {
                paths.push(args[i].clone());
                i += 1;
            }
            if !paths.is_empty() {
                return LaunchAction::VaultAdd(paths);
            }
        } else if a == "--library" {
            if let Some(dir) = args.get(i + 1) {
                if !dir.starts_with("--") {
                    return LaunchAction::Library(dir.clone());
                }
            }
        } else if !a.starts_with('-') {
            // 裸路径：扩展名匹配图片格式才响应（避免误吞任意参数）
            let ext = a.rsplit('.').next();
            let is_image = ext
                .map(|e| assoc::ASSOC_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
                .unwrap_or(false);
            if is_image {
                return LaunchAction::OpenImage(a.clone());
            }
            // 视频扩展 → 独立播放器（注册表关联注册见 assoc.rs VIDEO_ASSOC_EXTS）
            let is_video = ext
                .map(|e| VIDEO_LAUNCH_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
                .unwrap_or(false);
            if is_video {
                return LaunchAction::OpenVideo(a.clone());
            }
        }
        i += 1;
    }
    LaunchAction::None
}

/// singleinstance 队列转发用的公开包装
pub(crate) fn parse_launch_args_public(args: &[String]) -> LaunchAction {
    let a = parse_launch_args(args);
    log::info!("[trace] parse_launch_args: {:?} 项 → {}", args.len(), match &a {
        LaunchAction::None => "None",
        LaunchAction::OpenImage(_) => "OpenImage",
        LaunchAction::OpenVideo(_) => "OpenVideo",
        LaunchAction::Library(_) => "Library",
        LaunchAction::VaultAdd(_) => "VaultAdd",
    });
    a
}

/// 获取看图窗口：不存在则懒创建（viewer 已从 tauri.conf.json 预定义窗口移除，
/// 所有调起 viewer 的入口统一走此函数，按需建窗降低启动 webview 数）。
///
/// 返回 (窗口, 是否本次新建)。新建时前端 listener 尚未挂载，
/// 向其派发事件前需等待就绪（调用方约定：sleep 1500ms 防事件丢失）。
///
/// 窗口创建必须在主线程（崩溃教训见 record/commands.rs setup_immersive 注释）：
/// run_on_main_thread 在主线程调用时闭包同步执行、非主线程时经事件循环转发，
/// 两种调用方（托盘/热键回调 vs 命令线程）皆安全无死锁，mpsc 只等结果不占主线程。
fn get_or_create_viewer_window(
    app: &tauri::AppHandle,
) -> Result<(tauri::WebviewWindow, bool), String> {
    if let Some(w) = app.get_webview_window("viewer") {
        return Ok((w, false));
    }
    log::info!("看图窗口不存在，懒创建");
    let app2 = app.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    app.run_on_main_thread(move || {
        let r = (|| -> Result<(), String> {
            let w = WebviewWindowBuilder::new(
                &app2,
                "viewer",
                tauri::WebviewUrl::App("#/viewer".into()),
            )
            .title("jietu-hdr · 看图")
            .inner_size(960.0, 640.0)
            .min_inner_size(640.0, 480.0)
            .center()
            .resizable(true)
            .decorations(false)
            .transparent(true)
            .shadow(true)
            .visible(false)
            .build()
            .map_err(|e| format!("创建看图窗口失败: {}", e))?;
            // 与主窗口一致的 Mica 材质（前端挂载后 set_window_theme 会按当前主题覆盖）
            if let Ok(hwnd) = w.hwnd() {
                apply_mica(hwnd.0 as isize);
            }
            Ok(())
        })();
        let _ = tx.send(r);
    })
    .map_err(|e| format!("转发主线程创建看图窗口失败: {}", e))?;
    match rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e), // build 失败
        Err(_) => return Err("创建看图窗口失败：主线程任务意外丢弃".into()), // 发送端意外丢弃
    }
    app.get_webview_window("viewer")
        .map(|w| (w, true))
        .ok_or_else(|| "创建看图窗口失败：窗口未注册".to_string())
}

/// 执行启动动作：显示看图窗口并向前端派发对应事件
fn handle_launch_action(app: &tauri::AppHandle, action: LaunchAction) {
    // 无路径动作（用户重复双击 exe）→ 唤起主窗口（浏览器式聚焦既有实例）
    if matches!(action, LaunchAction::None) {
        if let Some(window) = app.get_webview_window("main") {
            // 最小化态必须 unminimize：仅 show() 不会还原窗口
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
        return;
    }
    // 视频文件（「打开方式」）→ 独立原生播放器窗口（PotPlayer 形态：
    // 自有窗口/30+ 键位/全屏，不与看图窗口共用——嵌入标签页方案已移除）
    if let LaunchAction::OpenVideo(p) = &action {
        log::info!("[video] OpenVideo 路由（独立播放器窗口）: {}", p);
        let path = p.clone();
        let app2 = app.clone();
        std::thread::Builder::new()
            .name("video-launch".into())
            .spawn(move || {
                let opts = video::commands::VideoPlayOpts {
                    seek: None,
                    volume: None,
                    rate: None,
                    borderless: None,
                    auto_exit: None,
                    subtitle: None,
                };
                if let Err(e) =
                    video::player::open_player(vec![std::path::PathBuf::from(path)], opts)
                {
                    log::error!("[video] 打开方式播放失败: {}", e);
                    let _ = app2.emit("video://error", e);
                }
            })
            .ok();
        return;
    }
    // viewer 按需获取/懒创建（窗口可能被旧版本销毁——现在 X 已改为隐藏）
    let (window, fresh) = match get_or_create_viewer_window(app) {
        Ok(w) => w,
        Err(e) => {
            log::error!("获取/创建看图窗口失败: {}", e);
            return;
        }
    };
    // 新建窗口前端 listener 尚未挂载，等待就绪再派发（防事件丢失）
    if fresh {
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    let _ = window.show();
    // 最小化态必须先还原，否则 set_focus 无效（双击同图"无反应"的根因）
    let _ = window.unminimize();
    let _ = window.set_focus();
    log::info!("[trace] handle_launch_action 窗口已显示，派发事件");
    // 事件名与 viewer://open 既有链路对齐，由 ViewerWindow.vue 统一监听
    match action {
        LaunchAction::None => {}
        LaunchAction::OpenVideo(_) => {} // 已在上方提前处理（独立播放器路径）
        LaunchAction::OpenImage(p) => {
            // payload 带 external 标记：关闭最后一个标签时据此决定回相册页还是隐藏窗口
            if let Err(e) = window.emit_to(
                "viewer",
                "viewer://open",
                serde_json::json!({ "path": p, "external": true }),
            ) {
                log::error!("[trace] viewer://open 派发失败: {}", e);
            }
        }
        LaunchAction::Library(dir) => {
            log::info!("[trace] 派发 viewer://library: {}", dir);
            // Rust 直调挂载相册源（不依赖前端事件往返，保证索引一定登记）
            if let Err(e) = crate::viewer::library::add_library_root(dir.clone()) {
                log::error!("[trace] add_library_root 失败: {}", e);
            }
            // 双发：立即一次 + 800ms 后一次（前端监听幂等；防 viewer webview 偶发未就绪丢事件）
            if let Err(e) = window.emit_to("viewer", "viewer://library", dir.clone()) {
                log::error!("[trace] viewer://library 派发失败: {}", e);
            }
            let w2 = window.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(800));
                let _ = w2.emit_to("viewer", "viewer://library", dir);
            });
        }
        LaunchAction::VaultAdd(paths) => {
            if let Err(e) = window.emit_to("viewer", "viewer://vault-add", paths) {
                log::error!("[trace] viewer://vault-add 派发失败: {}", e);
            }
        }
    }
}

/// singleinstance 队列转发用的公开包装
pub(crate) fn handle_launch_action_public(app: &tauri::AppHandle, action: LaunchAction) {
    handle_launch_action(app, action);
}

/// 主面板「打开图片」：选图后调起看图窗口（走 Rust emit_to，比 JS 跨窗口广播可靠）
#[tauri::command]
fn open_viewer_image(app: tauri::AppHandle, path: String) -> Result<(), String> {
    handle_launch_action(&app, LaunchAction::OpenImage(path));
    Ok(())
}

/// 隐藏看图窗口（外部来源打开的最后一个标签被关闭时调用——没开过相册就不回相册页）
#[tauri::command]
fn hide_viewer_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("viewer") {
        let _ = window.hide();
    }
    Ok(())
}

/// 前端追踪日志（写 jietu-hdr.log，用于跨窗口链路定位）
#[tauri::command]
fn trace_log(msg: String) {
    log::info!("[fe] {}", msg);
}

/// 主面板「AI 放大」：调起独立 AI 放大窗口（upscale 已从 tauri.conf.json 预定义
/// 窗口移除，首次调用时懒创建；创建后 X 关闭为隐藏，复用零加载延迟）。
/// 窗口创建转发主线程（同 get_or_create_viewer_window 模式，防命令线程 build panic）。
#[tauri::command]
fn open_upscale_window(app: tauri::AppHandle) -> Result<(), String> {
    if app.get_webview_window("upscale").is_none() {
        log::info!("AI 放大窗口不存在，懒创建");
        let app2 = app.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), tauri::Error>>();
        app.run_on_main_thread(move || {
            let r = WebviewWindowBuilder::new(
                &app2,
                "upscale",
                tauri::WebviewUrl::App("#/upscale".into()),
            )
            .title("jietu-hdr · AI 放大")
            .inner_size(580.0, 660.0)
            .min_inner_size(500.0, 560.0)
            .center()
            .resizable(true)
            .decorations(false)
            .transparent(true)
            .shadow(true)
            .visible(false)
            .build()
            .map(|_| ());
            let _ = tx.send(r);
        })
        .map_err(|e| format!("创建 AI 放大窗口失败: {}", e))?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(format!("创建 AI 放大窗口失败: {}", e)),
            Err(_) => return Err("创建 AI 放大窗口失败：主线程任务意外丢弃".into()),
        }
    }
    let Some(window) = app.get_webview_window("upscale") else {
        return Err("AI 放大窗口创建失败".into());
    };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    Ok(())
}

/// 主面板「打开相册」：直接调起看图窗口相册页（不弹文件夹选择框，浏览已挂载的相册源）
#[tauri::command]
fn open_viewer_library(app: tauri::AppHandle) -> Result<(), String> {
    log::info!("[trace] open_viewer_library 收到调用");
    // 窗口按需懒创建；新建时默认视图即相册页，无需补发 viewer://album
    let (window, fresh) = get_or_create_viewer_window(&app)?;
    let _ = window.show();
    let _ = window.set_focus();
    if fresh {
        log::info!("[trace] viewer 窗口已懒创建（默认相册页）");
        return Ok(());
    }
    if let Err(e) = window.emit_to("viewer", "viewer://album", ()) {
        log::error!("[trace] viewer://album 派发失败: {}", e);
    }
    Ok(())
}

/// 导出配置到指定文件（TOML 副本）
#[tauri::command]
fn export_config(path: String) -> Result<(), String> {
    let cfg = Config::load();
    let content = toml::to_string_pretty(&cfg).map_err(|e| format!("{:#}", e))?;
    std::fs::write(&path, content).map_err(|e| format!("{:#}", e))?;
    log::info!("配置已导出: {}", path);
    Ok(())
}

/// 从指定文件导入配置（解析 → 保存 → 重注册热键）
#[tauri::command]
fn import_config(app: tauri::AppHandle, path: String) -> Result<Config, String> {
    let content = std::fs::read_to_string(&path).map_err(|e| format!("{:#}", e))?;
    let cfg: Config = toml::from_str(&content).map_err(|e| format!("配置文件解析失败: {:#}", e))?;
    cfg.save().map_err(|e| format!("{:#}", e))?;
    sync_hotkeys(app, &cfg).map_err(|e| format!("{:#}", e))?;
    log::info!("配置已导入: {}", path);
    Ok(cfg)
}

/// 一键备份配置到 %APPDATA%\jietu-hdr\backups\，返回备份文件路径
#[tauri::command]
fn backup_config() -> Result<String, String> {
    let cfg = Config::load();
    let base = dirs::config_dir()
        .map(|d| d.join("jietu-hdr").join("backups"))
        .ok_or_else(|| "无法定位配置目录".to_string())?;
    std::fs::create_dir_all(&base).map_err(|e| format!("{:#}", e))?;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = base.join(format!("jietu-config_{}.toml", ts));
    let content = toml::to_string_pretty(&cfg).map_err(|e| format!("{:#}", e))?;
    std::fs::write(&path, content).map_err(|e| format!("{:#}", e))?;
    log::info!("配置已备份: {}", path.display());
    Ok(path.to_string_lossy().into_owned())
}

/// 热键类型 → 运行时状态读写
fn hotkey_of_state(st: &HotkeyState, kind: &str) -> Result<Option<Hotkey>, String> {
    match kind {
        "region" => Ok(st.region),
        "fullscreen" => Ok(st.fullscreen),
        "silent" => Ok(st.silent),
        "clean" => Ok(st.clean),
        "record_start" => Ok(st.record_start),
        "record_stop" => Ok(st.record_stop),
        _ => Err(format!("未知热键类型: {}", kind)),
    }
}
fn set_hotkey_of_state(st: &mut HotkeyState, kind: &str, hk: Hotkey) -> Result<(), String> {
    match kind {
        "region" => st.region = Some(hk),
        "fullscreen" => st.fullscreen = Some(hk),
        "silent" => st.silent = Some(hk),
        "clean" => st.clean = Some(hk),
        "record_start" => st.record_start = Some(hk),
        "record_stop" => st.record_stop = Some(hk),
        _ => return Err(format!("未知热键类型: {}", kind)),
    }
    Ok(())
}

/// 注册单个热键（前端用于即时试注册或换绑）
///
/// 冲突检测流程（保证旧热键不受影响）：
/// 1. 新旧热键相同 → 幂等成功
/// 2. 先尝试注册新热键：失败 = 被其他程序/本应用其他功能占用 → 返回冲突错误
/// 3. 新热键注册成功后才注销旧热键并更新状态
///
/// - `kind`: "region"（区域截图）| "fullscreen"（全屏截图）| "silent"（静默截图）
/// - `hotkey`: 配置中的 Hotkey 结构
#[tauri::command]
fn register_hotkey(app: tauri::AppHandle, kind: String, hotkey: Hotkey) -> Result<(), String> {
    let sc = hotkey_to_shortcut(&hotkey)
        .ok_or_else(|| format!("不支持的热键 vk=0x{:02X}", hotkey.vk))?;
    let gs = app.global_shortcut();

    // 读取当前运行时旧热键
    let old_hk = {
        let state = app
            .try_state::<Mutex<HotkeyState>>()
            .ok_or_else(|| "热键状态未初始化".to_string())?;
        let st = state.lock().map_err(|e| format!("状态锁失败: {}", e))?;
        hotkey_of_state(&st, &kind)?
    };

    // 幂等：新旧相同 → 直接成功
    if old_hk == Some(hotkey) {
        log::info!(
            "{}热键未变化，跳过注册: mods=0x{:X} vk=0x{:02X}",
            kind,
            hotkey.modifiers,
            hotkey.vk
        );
        return Ok(());
    }

    // 冲突检测：先注册新热键（此时旧热键仍生效）
    let desc = sc.into_string();
    if let Err(e) = gs.register(sc) {
        log::warn!("热键冲突检测失败 ({}): {} —— 已被占用", desc, e);
        return Err(format!(
            "热键 [{}] 已被占用（其他程序或本应用其他功能），请更换组合",
            desc
        ));
    }

    // 新热键注册成功 → 注销旧热键（失败不阻断，仅记录）
    if let Some(old) = old_hk {
        if let Some(old_sc) = hotkey_to_shortcut(&old) {
            if let Err(e) = gs.unregister(old_sc) {
                log::warn!("注销旧热键失败: {}", e);
            }
        }
    }

    // 更新运行时状态
    if let Some(state) = app.try_state::<Mutex<HotkeyState>>() {
        let mut st = state.lock().map_err(|e| format!("状态锁失败: {}", e))?;
        set_hotkey_of_state(&mut st, &kind, hotkey)?;
    }
    log::info!(
        "已注册 {} 热键: mods=0x{:X} vk=0x{:02X}",
        kind,
        hotkey.modifiers,
        hotkey.vk
    );
    Ok(())
}

/// 注销单个热键
#[tauri::command]
fn unregister_hotkey(app: tauri::AppHandle, hotkey: Hotkey) -> Result<(), String> {
    if let Some(sc) = hotkey_to_shortcut(&hotkey) {
        app.global_shortcut()
            .unregister(sc)
            .map_err(|e| format!("注销热键失败: {}", e))?;
    }
    Ok(())
}

/// 根据配置同步注册的全局热键（注销旧的 → 注册新的）
///
/// 容错策略：单个热键注册失败（如被系统其他程序占用）只记录 warning，不阻断启动
fn sync_hotkeys(app: tauri::AppHandle, config: &Config) -> anyhow::Result<()> {
    // 更新运行时状态
    if let Some(state) = app.try_state::<Mutex<HotkeyState>>() {
        let mut st = state
            .lock()
            .map_err(|e| anyhow::anyhow!("状态锁失败: {}", e))?;
        st.region = Some(config.region_hotkey);
        st.fullscreen = Some(config.fullscreen_hotkey);
        st.silent = Some(config.silent_hotkey);
        st.clean = config.memory_clean.hotkey;
        st.record_start = config.record_start_hotkey;
        st.record_stop = config.record_stop_hotkey;
        st.video_start = config.video_start_hotkey;
        st.video_stop = config.video_stop_hotkey;
    }
    reregister_all_hotkeys(&app);
    log::info!("热键同步完成");
    Ok(())
}

/// 注销全部已注册热键，再从运行时状态（HotkeyState）重新注册
///
/// 每个热键独立容错：注册失败只记 warning（如被系统/其他程序占用）
fn reregister_all_hotkeys(app: &tauri::AppHandle) {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();

    // 读取运行时状态中的热键（含清理内存与录制/视频）
    let hotkeys: [(String, Option<Hotkey>); 6] = {
        let state = match app.try_state::<Mutex<HotkeyState>>() {
            Some(s) => s,
            None => return,
        };
        let st = match state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        [
            ("区域截图".into(), st.region),
            ("全屏截图".into(), st.fullscreen),
            ("静默截图".into(), st.silent),
            ("清理内存".into(), st.clean),
            ("开始录制".into(), st.record_start),
            ("开始录制视频".into(), st.video_start),
        ]
    };

    for (name, hk) in hotkeys
        .iter()
        .filter_map(|(n, h)| h.map(|h| (n.clone(), h)))
    {
        match hotkey_to_shortcut(&hk) {
            Some(sc) => {
                let desc = sc.into_string();
                if let Err(e) = gs.register(sc) {
                    log::warn!(
                        "注册{}热键失败 ({}): {} —— 可能已被其他程序占用，请在设置中更换",
                        name,
                        desc,
                        e
                    );
                } else {
                    log::info!("已注册{}热键: {}", name, desc);
                }
            }
            None => {
                log::warn!("{}热键 vk=0x{:02X} 无法映射为合法 Code，跳过", name, hk.vk);
            }
        }
    }

    // 固定热键：Ctrl+Shift+L 唤起隐私相册（设计稿 5.2，不走可配置体系）
    let vault_sc = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyL);
    if let Err(e) = gs.register(vault_sc) {
        log::warn!(
            "注册隐私相册热键失败 (Ctrl+Shift+L): {} —— 可能已被其他程序占用",
            e
        );
    }

    // 开始录制热键已在上方列表中按配置注册（None = 不注册）；Alt+F10 停止录制
    // 由沉浸式 setup_immersive 在录制期间临时注册/注销，不在此处——但若此刻
    // 正在录制（配置保存/热键录入会触发本函数 unregister_all），需补注册回去
    record::commands::reregister_stop_hotkey_if_active(app);
    // 视频录制同理：Alt+F8 停止热键由 video 沉浸式临时注册，unregister_all 后补回
    video::commands::reregister_video_stop_hotkey_if_active(app);
}

/// 热键录入模式：临时注销全部全局热键，避免录入的按键组合被自身 RegisterHotKey 拦截
///
/// 前端流程：开始录入 → enabled=true（此期间任何组合键都能到达 WebView）
///          录入完成/取消 → enabled=false（按 HotkeyState 重新注册全部热键）
#[tauri::command]
fn set_hotkey_capture_mode(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    if enabled {
        let _ = app.global_shortcut().unregister_all();
        log::info!("热键录入模式：已临时注销全部全局热键");
    } else {
        reregister_all_hotkeys(&app);
        log::info!("热键录入模式：结束，已恢复全部热键注册");
    }
    Ok(())
}

/// 启用开机自启
#[tauri::command]
fn enable_autostart(app: tauri::AppHandle) -> Result<(), String> {
    app.autolaunch()
        .enable()
        .map_err(|e| format!("启用开机自启失败: {}", e))
}

/// 关闭开机自启（幂等：注册表项不存在时 auto-launch 会报 os error 2，
/// 先查询状态，未启用直接成功——常见于旧版本以不同 exe 名注册后失效）
#[tauri::command]
fn disable_autostart(app: tauri::AppHandle) -> Result<(), String> {
    match app.autolaunch().is_enabled() {
        Ok(false) => Ok(()),
        Ok(true) => app
            .autolaunch()
            .disable()
            .map_err(|e| format!("关闭开机自启失败: {}", e)),
        Err(e) => Err(format!("查询开机自启状态失败: {}", e)),
    }
}

/// 查询开机自启是否已启用
#[tauri::command]
fn is_autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    app.autolaunch()
        .is_enabled()
        .map_err(|e| format!("查询开机自启状态失败: {}", e))
}

/// 显示主窗口（从托盘恢复；若 region-select 覆盖层残留则顺带收起，防吞输入）
#[tauri::command]
fn show_main_window(app: tauri::AppHandle) -> Result<(), String> {
    hide_region_select_if_visible(&app);
    if let Some(window) = app.get_webview_window("main") {
        window.show().map_err(|e| format!("显示窗口失败: {}", e))?;
        let _ = window.set_focus();
    }
    Ok(())
}

/// 主面板/设置中心视图切换时控制几何录制（设置页期间暂停，防止设置页尺寸覆盖记忆）
#[tauri::command]
fn set_main_geometry_recording(enabled: bool) {
    MAIN_WIN_RECORDING.store(enabled, Ordering::Relaxed);
}

/// 退出应用（前端「退出」按钮和托盘菜单共享）
#[tauri::command]
fn quit_app(app: tauri::AppHandle) -> Result<(), String> {
    quit_app_path(app)
}

/// 退出应用核心（quit_app 命令 + video 完成动作 "exit" 共用）
pub fn quit_app_path(app: tauri::AppHandle) -> Result<(), String> {
    flush_main_window_state();
    // 退出保护：有录制会话/编码收尾时同步等待保存完成，防录制产物截断
    record::commands::flush_on_exit(&app);
    // 视频会话退出保护（nvenc 排空 + MKV Cues 回填秒级；无会话立即返回）
    video::commands::flush_on_exit();
    app.exit(0);
    Ok(())
}

/// 前端日志转发（写入 jietu-hdr.log，带 [FE] 前缀）
#[tauri::command]
fn frontend_log(msg: String) {
    log::info!("[FE] {}", msg);
}

/// 获取系统临时目录路径（去除尾部反斜杠，避免拼接出双反斜杠）
#[tauri::command]
fn get_temp_dir() -> String {
    let mut p = std::env::temp_dir().to_string_lossy().into_owned();
    while p.ends_with('\\') || p.ends_with('/') {
        p.pop();
    }
    p
}

/// 查询所有显示器的 HDR 状态（支持与否、开启与否）
#[tauri::command]
fn get_hdr_status() -> Result<Vec<capture::HdrMonitorState>, String> {
    capture::hdr_toggle::get_hdr_states()
}

/// 切换 HDR 开关
///
/// - `device_name`: 目标显示器（如 "\\.\DISPLAY1"）；None 时应用到所有支持 HDR 的显示器
/// - `enable`: true 开启 / false 关闭
#[tauri::command]
fn set_hdr_state(device_name: Option<String>, enable: bool) -> Result<usize, String> {
    capture::hdr_toggle::set_hdr_state(device_name.as_deref(), enable)
}

/// 设置指定显示器的 HDR 内容亮度（SDR 白电平，nits）
#[tauri::command]
fn set_sdr_white_level(device_name: String, nits: f32) -> Result<(), String> {
    capture::hdr_toggle::set_sdr_white_level(&device_name, nits)
}

/// 查询所有显示器的亮度（DDC/CI）
#[tauri::command]
fn get_brightness_status() -> Result<Vec<capture::MonitorBrightness>, String> {
    capture::brightness::get_brightness_states()
}

/// 设置指定显示器亮度（0-100）
#[tauri::command]
fn set_brightness(device_name: String, level: u32) -> Result<(), String> {
    capture::brightness::set_brightness(&device_name, level)
}

/// 简单窗口信息（用于前端选择）
#[derive(Clone, Debug, serde::Serialize)]
struct WindowInfo {
    hwnd: isize,
    title: String,
    width: i32,
    height: i32,
    left: i32,
    top: i32,
    /// 自绘边框窗口（QQ/微信 NT/浏览器等）：主体流程做像素级内吸附检测
    #[serde(skip)]
    custom_frame: bool,
}

/// 枚举所有可见的顶级窗口（带标题），返回给前端选择
///
/// 窗口矩形使用 DWM 扩展帧边界（DWMWA_EXTENDED_FRAME_BOUNDS），
/// 这是不含阴影的真实可见区域，与微信/QQ 截图的窗口识别一致。
/// EnumWindows 按 z 序从顶层往下返回。
///
/// 坐标基准：通过 MapWindowPoints 把屏幕物理坐标转换为
/// **region-select 覆盖层客户区坐标**（物理像素），
/// 前端直接除以 devicePixelRatio 即为 CSS 坐标，
/// 避免 outerPosition（GetWindowRect 含边框溢出）造成的吸附偏移。
#[tauri::command]
fn list_windows(app: tauri::AppHandle) -> Vec<WindowInfo> {
    use std::sync::Mutex;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT, TRUE};
    use windows::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    };
    use windows::Win32::Graphics::Gdi::MapWindowPoints;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, IsWindowVisible, GWL_EXSTYLE, GWL_STYLE, WS_CAPTION, WS_EX_TOOLWINDOW,
    };

    static COLLECTED: Mutex<Vec<WindowInfo>> = Mutex::new(Vec::new());

    // 清空之前的收集
    if let Ok(mut v) = COLLECTED.lock() {
        v.clear();
    }

    // 覆盖层窗口句柄（通过 LPARAM 传给回调；0 = 无覆盖层，退回屏幕坐标）
    let overlay_hwnd: isize = app
        .get_webview_window("region-select")
        .and_then(|w| w.hwnd().ok())
        .map(|h| h.0 as isize)
        .unwrap_or(0);

    extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return TRUE;
            }
            // 跳过 cloaked 窗口（UWP 后台实例、其他虚拟桌面上的窗口等）
            let mut cloaked: u32 = 0;
            let cloaked_ok = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut _ as *mut _,
                std::mem::size_of::<u32>() as u32,
            )
            .is_ok();
            if cloaked_ok && cloaked != 0 {
                return TRUE;
            }
            // 跳过工具窗口（浮动小面板，微信/QQ 截图窗口识别也不包含它们）
            let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
            if ex_style & (WS_EX_TOOLWINDOW.0 as i32) != 0 {
                return TRUE;
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return TRUE;
            }
            let mut buf = vec![0u16; (len as usize) + 1];
            let copied = GetWindowTextW(hwnd, &mut buf);
            if copied == 0 {
                return TRUE;
            }
            let title = String::from_utf16_lossy(&buf[..copied as usize]);
            if title.trim().is_empty() {
                return TRUE;
            }
            // 优先 DWM 真实边界（不含阴影），失败时退回 GetWindowRect
            let mut rect = RECT::default();
            let dwm_ok = DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rect as *mut _ as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok();
            if !dwm_ok {
                if GetWindowRect(hwnd, &mut rect).is_err() {
                    return TRUE;
                }
            }
            // 自绘边框窗口（QQ/微信 NT 系）：DWM 扩展边界含自绘的透明外边/
            // 圆角/柔和阴影一圈，吸附时多截窗口外一层。
            // 检测：客户区是否铺满 DWM 可见边界——标准窗口客户区明显小于
            // DWM 边界（差出标题栏 ~31px + 边框 ~8px），自绘窗口差值 ≈ 0。
            // 实测 QQ NT（「会话」主窗 style=0x960F0000）不置 WS_CAPTION
            // 但客户区铺满 DWM 边界——故不能要求 WS_CAPTION。
            // 游戏误裁风险（WS_POPUP 客户区也铺满）改由主体两层防护：
            //   1) 全屏窗口跳过内缩（覆盖整个显示器 = 无边框全屏游戏）
            //   2) WS_POPUP 且无 WS_SIZEBOX/WS_THICKFRAME 的非全屏窗口
            //      （小窗口化游戏）像素扫描检测其阴影环宽度通常为 0-1px，
            //      夹紧下限 2px 不会误裁明显内容
            // 命中后不在此处内缩，标记 custom_frame 由主体做像素级内吸附检测。
            let mut crect = RECT::default();
            let custom_frame = GetClientRect(hwnd, &mut crect).is_ok() && {
                let mut pts = [
                    POINT { x: 0, y: 0 },
                    POINT {
                        x: crect.right,
                        y: crect.bottom,
                    },
                ];
                // 客户区坐标 → 屏幕物理坐标
                MapWindowPoints(hwnd, HWND::default(), &mut pts);
                (pts[0].x - rect.left).max(0) <= 2
                    && (pts[0].y - rect.top).max(0) <= 2
                    && (rect.right - pts[1].x).max(0) <= 2
                    && (rect.bottom - pts[1].y).max(0) <= 2
            };
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if w < 50 || h < 50 {
                return TRUE;
            }
            let _ = lparam;
            // 诊断：所有候选窗口打印 custom_frame 判定细节（定位 QQ 类窗口漏判）
            if let Ok(mut v) = COLLECTED.lock() {
                log::info!(
                    "窗口枚举: 「{}」({}x{}) custom_frame={} style=0x{:08X}",
                    title,
                    w,
                    h,
                    custom_frame,
                    GetWindowLongW(hwnd, GWL_STYLE) as u32
                );
                v.push(WindowInfo {
                    hwnd: hwnd.0 as isize,
                    title,
                    width: w,
                    height: h,
                    left: rect.left,
                    top: rect.top,
                    custom_frame,
                });
            }
        }
        TRUE
    }

    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
    }
    let mut list = COLLECTED.lock().map(|v| v.clone()).unwrap_or_default();

    // === 自绘窗口像素级内吸附：扫描边缘阴影渐变，找内容真实起点 ===
    // 固定内缩对 QQ 这类窗口不够准（阴影宽度因窗口/主题而异）；
    // 直接截取窗口区域像素，从每边向内扫描颜色稳定区（QQ 阴影混合背景
    // 呈渐变，进入内容区后颜色稳定），各边独立测出真实内缩量。
    let has_custom = list.iter().any(|w| w.custom_frame);
    if has_custom {
        // 全屏窗口判定基准（覆盖整个显示器的窗口 = 无边框全屏游戏，无阴影可吸附）
        let monitors = capture::monitor::enumerate_monitors().unwrap_or_default();
        // BitBlt 会把 EXCLUDEFROMCAPTURE 的全屏覆盖层涂黑 → 检测前临时隐藏
        let overlay = app.get_webview_window("region-select");
        let overlay_hidden = overlay
            .as_ref()
            .map(|o| o.is_visible().unwrap_or(false))
            .unwrap_or(false);
        if let Some(o) = overlay.as_ref() {
            if overlay_hidden {
                let _ = o.hide();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        for w in list.iter_mut() {
            if !w.custom_frame {
                continue;
            }
            // 全屏窗口：游戏内容延伸到窗口四边，无自绘阴影，跳过内缩
            let fullscreen = monitors.iter().any(|m| {
                w.left <= m.left
                    && w.top <= m.top
                    && w.left + w.width >= m.left + m.width as i32
                    && w.top + w.height >= m.top + m.height as i32
            });
            if fullscreen {
                log::info!(
                    "窗口识别: 全屏窗口「{}」({},{},{}x{}) 跳过内吸附",
                    w.title,
                    w.left,
                    w.top,
                    w.width,
                    w.height
                );
                continue;
            }
            let rect = RECT {
                left: w.left,
                top: w.top,
                right: w.left + w.width,
                bottom: w.top + w.height,
            };
            let (il, it, ir, ib) = detect_frame_inset(&rect).unwrap_or((8, 8, 8, 8));
            log::info!(
                "窗口识别: 自绘「{}」像素内吸附 l{} t{} r{} b{}",
                w.title,
                il,
                it,
                ir,
                ib
            );
            w.left += il;
            w.top += it;
            w.width -= il + ir;
            w.height -= it + ib;
        }
        if let Some(o) = overlay.as_ref() {
            if overlay_hidden {
                let _ = o.show();
                let _ = o.set_focus();
            }
        }
    }

    // 屏幕物理坐标 → 覆盖层客户区物理坐标（消除边框溢出偏移）
    if overlay_hwnd != 0 {
        for w in list.iter_mut() {
            let mut pts = [POINT {
                x: w.left,
                y: w.top,
            }];
            unsafe {
                MapWindowPoints(HWND::default(), HWND(overlay_hwnd as *mut _), &mut pts);
            }
            w.left = pts[0].x;
            w.top = pts[0].y;
        }
    }

    list
}

/// 像素级检测窗口边缘阴影/透明边宽度（自绘窗口内吸附）
///
/// GDI 截取窗口矩形（屏幕合成画面），从每边向内扫描：
/// 每条采样线以【边内 30px 深处的该线像素】为参考色（顶部取标题栏中部 20px），
/// 找第一条"连续 6px 接近参考色"的扫描线位置——阴影区颜色渐变不稳定，
/// 进入内容区后稳定。每边 9 条采样线取中位数（过滤命中文字/头像的异常线）。
///
/// 深参考的原因：固定 12px 浅参考在高 DPI 缩放下（阴影环 ~13-17px）
/// 会落在阴影内部，扫描从 p=0 即误匹配 → inset=2 → 阴影环残留。
/// 返回 (left, top, right, bottom) 各边内缩量；失败返回 None（调用方用默认 8）。
fn detect_frame_inset(rect: &windows::Win32::Foundation::RECT) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HGDIOBJ,
        SRCCOPY,
    };

    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    // 尺寸限制：太小无意义，太大开销高（>8MP 用默认内缩）
    if !(50..=3200).contains(&w) || !(50..=3200).contains(&h) {
        return None;
    }
    if (w as i64) * (h as i64) > 8_000_000 {
        return None;
    }

    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, HGDIOBJ::from(bmp));
        BitBlt(mem, 0, 0, w, h, screen, rect.left, rect.top, SRCCOPY);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 顶-底行序
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut data = vec![0u8; (w * h * 4) as usize];
        let got = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(data.as_mut_ptr().cast()),
            &mut bi,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ::from(bmp));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if got != h {
            return None;
        }

        // 像素取色（BGRA → RGB）
        let px = |x: i32, y: i32| -> [u8; 3] {
            let o = ((y * w + x) * 4) as usize;
            [data[o + 2], data[o + 1], data[o]]
        };
        // 颜色容差 12（严）：QQ 白窗口叠浅色壁纸时阴影环与内容色差仅 ~15，
        // 旧容差 28 会把阴影误判为内容（p=0 即命中 → inset=2 阴影残留）。
        // 12 可滤掉阴影渐变尾段，同时 Edge 1px 深色边框（差 >100）不受影响。
        let close = |a: [u8; 3], b: [u8; 3]| -> bool {
            (a[0] as i32 - b[0] as i32).abs() < 12
                && (a[1] as i32 - b[1] as i32).abs() < 12
                && (a[2] as i32 - b[2] as i32).abs() < 12
        };
        // 从 (x0,y0) 沿 (dx,dy) 扫描：第一个"连续 6px 接近参考色"的位置。
        // 范围 0..25：run 最大覆盖 [24,29]，不触及 30px 深参考点（防自匹配）
        let scan = |x0: i32, y0: i32, dx: i32, dy: i32, r: [u8; 3]| -> Option<i32> {
            for p in 0..25i32 {
                let mut ok = true;
                for q in 0..6i32 {
                    let x = x0 + (p + q) * dx;
                    let y = y0 + (p + q) * dy;
                    if x < 0 || y < 0 || x >= w || y >= h || !close(px(x, y), r) {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    return Some(p);
                }
            }
            None
        };
        let median = |v: &mut Vec<i32>| -> Option<i32> {
            if v.is_empty() {
                return None;
            }
            v.sort_unstable();
            Some(v[v.len() / 2])
        };
        // 检测值夹紧 [2, 24]：下限补偿圆角，上限防误判；失败默认 8
        let clamp = |v: Option<i32>| -> i32 { v.map(|p| p.clamp(2, 24)).unwrap_or(8) };

        const N: i32 = 9; // 每边采样线数（均布 10%-90%，避开圆角）
                          // 参考位置：左/右/下取边内 30px（越过任何 DPI 缩放下 ~17px 的阴影环），
                          // 顶部取 20px（标题栏中部：浅于阴影、深于标题栏底部分隔渐变）。
                          // 每条采样线用【该线自己的深参考色】——共享单点参考会被标题文字/
                          // 头像等内容污染（旧实现顶参考取 w/2 正中恒命中标题文字）；
                          // 命中异常内容的线返回 None，由 9 线中位数过滤。
        const DEEP: i32 = 30;
        const TOP_REF: i32 = 20;
        let mut vals = Vec::new();

        // 左：每线参考 = 该线 (30, y) 的内容色
        for i in 0..N {
            let y = h * (i + 1) / (N + 1);
            let r = px(DEEP, y);
            if let Some(p) = scan(0, y, 1, 0, r) {
                vals.push(p);
            }
        }
        let il = clamp(median(&mut vals));

        // 右：每线参考 = 该线 (w-31, y)
        vals.clear();
        for i in 0..N {
            let y = h * (i + 1) / (N + 1);
            let r = px(w - 1 - DEEP, y);
            if let Some(p) = scan(w - 1, y, -1, 0, r) {
                vals.push(p);
            }
        }
        let ir = clamp(median(&mut vals));

        // 上：每线参考 = 该列 (x, 20)（标题栏中部，QQ 标题栏 ~40px 高各 DPI 安全）
        vals.clear();
        for i in 0..N {
            let x = w * (i + 1) / (N + 1);
            let r = px(x, TOP_REF);
            if let Some(p) = scan(x, 0, 0, 1, r) {
                vals.push(p);
            }
        }
        let it = clamp(median(&mut vals));

        // 下：每线参考 = 该列 (x, h-31)
        vals.clear();
        for i in 0..N {
            let x = w * (i + 1) / (N + 1);
            let r = px(x, h - 1 - DEEP);
            if let Some(p) = scan(x, h - 1, 0, -1, r) {
                vals.push(p);
            }
        }
        let ib = clamp(median(&mut vals));

        Some((il, it, ir, ib))
    }
}

/// OCR 文字识别：对指定 PNG 图片执行系统 OCR，返回识别的文字行
///
/// 坐标为图像像素（与图片尺寸一致），前端按选区缩放映射到屏幕位置。
/// 可用于"识别截图中的文字并选择复制"功能。
/// `language`: None/空 = 自动选择（zh-Hans 优先）；否则使用指定 BCP-47 标签
#[tauri::command]
fn ocr_image(
    png_path: String,
    language: Option<String>,
) -> Result<Vec<capture::OcrLineInfo>, String> {
    let lang = language.filter(|s| !s.trim().is_empty());
    log::info!("OCR 识别: {} (lang={:?})", png_path, lang);
    capture::ocr::run_ocr(&png_path, lang.as_deref())
}

/// 列出系统已安装的 OCR 识别语言（BCP-47 标签），供前端下拉选择
#[tauri::command]
fn ocr_languages() -> Result<Vec<String>, String> {
    capture::ocr::available_languages()
}

/// 标注 PNG → scRGB f16 捕获纹理（sRGB EOTF 解码 → 线性光 f16）
///
/// 标注画布是 SDR RGBA8（sRGB）；转成 scRGB 线性光后可复用
/// HDR 编码器（save_hdr_png / save_exr），输出"SDR 内容装 HDR 容器"。
fn annotated_png_to_texture(png_bytes: &[u8]) -> anyhow::Result<capture::CapturedTexture> {
    let img = image::load_from_memory(png_bytes)?;
    let rgba = img.to_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    let src = rgba.as_raw();

    let npx = (width as usize) * (height as usize);
    let mut data = vec![0u8; npx * 8]; // 4 × f16 = 8 bytes/px
    for i in 0..npx {
        let r = crate::color::srgb_eotf(src[i * 4] as f32 / 255.0);
        let g = crate::color::srgb_eotf(src[i * 4 + 1] as f32 / 255.0);
        let b = crate::color::srgb_eotf(src[i * 4 + 2] as f32 / 255.0);
        data[i * 8..i * 8 + 2].copy_from_slice(&f32_to_f16_bytes(r));
        data[i * 8 + 2..i * 8 + 4].copy_from_slice(&f32_to_f16_bytes(g));
        data[i * 8 + 4..i * 8 + 6].copy_from_slice(&f32_to_f16_bytes(b));
        // A 固定 1.0（f16 0x3C00）
        data[i * 8 + 6..i * 8 + 8].copy_from_slice(&0x3C00u16.to_le_bytes());
    }

    Ok(capture::CapturedTexture {
        width,
        height,
        format: color::PixelFormat::R16g16b16a16Float,
        data,
        row_pitch: width as usize * 8,
        via_gdi: false,
    })
}

/// f32 → f16（半精度）小端字节（标注内容为 [0,1] 区间，简化转换足够）
fn f32_to_f16_bytes(v: f32) -> [u8; 2] {
    fn to_u16(v: f32) -> u16 {
        let x = v.to_bits();
        let sign = ((x >> 16) & 0x8000) as u16;
        let exp32 = ((x >> 23) & 0xff) as i32;
        let mantissa = x & 0x007f_ffff;

        if exp32 == 0xff {
            // Inf / NaN
            return sign | 0x7c00 | if mantissa != 0 { 0x0200 } else { 0 };
        }
        if exp32 == 0 {
            // f32 次正规/零 → f16 零（sRGB 解码后不出现次正规）
            return sign;
        }
        let exp16 = exp32 - 127 + 15;
        if exp16 >= 0x1f {
            // 溢出 → f16 最大有限值（钳制）
            return sign | 0x7bff;
        }
        if exp16 <= 0 {
            // 太小 → 0
            return sign;
        }
        let m = (mantissa >> 13) as u16;
        sign | ((exp16 as u16) << 10) | m
    }
    to_u16(v).to_le_bytes()
}

/// 将 HDR 捕获纹理按配置格式直存（区域截图无标注保存 = 真 HDR 截图）
///
/// 与 capture_silent_inner 的分发一致：PngHdr/Jxl/Jxr/Exr 从 scRGB 纹理
/// 直编（保留真实 nits），PngSdr/Avif 色调映射后存。返回实际写入路径。
fn save_captured_as_format(
    captured: &capture::CapturedTexture,
    format: OutputFormat,
    path: &std::path::Path,
    params: &HdrToSdrParams,
    quality: encode::QualityLevel,
    output_depth: u32,
) -> anyhow::Result<std::path::PathBuf> {
    match format {
        OutputFormat::PngHdr => {
            encode::save_hdr_png(captured, path, params)?;
            Ok(path.to_path_buf())
        }
        OutputFormat::Jxl => {
            #[cfg(feature = "jxl")]
            {
                encode::save_jxl(captured, path, params, quality, output_depth)?;
                Ok(path.to_path_buf())
            }
            #[cfg(not(feature = "jxl"))]
            {
                let p = path.with_extension("png");
                let sdr = to_sdr(captured, params);
                encode::save_sdr_png(&sdr, &p)?;
                Ok(p)
            }
        }
        OutputFormat::Jxr => {
            encode::save_jxr(captured, path, params, quality)?;
            Ok(path.to_path_buf())
        }
        OutputFormat::Exr => {
            encode::save_exr(captured, path)?;
            Ok(path.to_path_buf())
        }
        _ => {
            let p = path.with_extension("png");
            let sdr = to_sdr(captured, params);
            encode::save_sdr_png(&sdr, &p)?;
            Ok(p)
        }
    }
}

/// 将标注 PNG 字节按指定格式保存到路径
///
/// - `PngSdr`：PNG 字节直接落盘（无损，无重编码）
/// - `PngHdr`：转 scRGB → 16bit PQ PNG + cICP（BT.2020）
/// - `Exr`：转 scRGB → 32bit 浮点 EXR
/// - `Jxl` / `Avif`：feature 未启用时回退 PNG（并修正扩展名）
///
/// 返回实际写入的路径（扩展名可能与传入 path 不同——回退时）。
fn save_annotated_as_format(
    png_bytes: &[u8],
    format: OutputFormat,
    path: &std::path::Path,
    quality: encode::QualityLevel,
    output_depth: u32,
) -> anyhow::Result<std::path::PathBuf> {
    match format {
        OutputFormat::PngSdr => {
            std::fs::write(path, png_bytes)?;
            Ok(path.to_path_buf())
        }
        OutputFormat::PngHdr => {
            let texture = annotated_png_to_texture(png_bytes)?;
            // scRGB 1.0 = SDR 白电平（与 Windows HDR 下 SDR 内容显示亮度一致）
            let params = HdrToSdrParams {
                input_sdr_white_nits: capture::monitor::get_sdr_white_level_nits(),
                ..Default::default()
            };
            encode::save_hdr_png(&texture, path, &params)?;
            Ok(path.to_path_buf())
        }
        OutputFormat::Exr => {
            let texture = annotated_png_to_texture(png_bytes)?;
            encode::save_exr(&texture, path)?;
            Ok(path.to_path_buf())
        }
        OutputFormat::Jxl => {
            // 标注图（SDR 内容）→ scRGB 纹理 → 16bit PQ JXL（SDR 内容装 HDR 容器）
            #[cfg(feature = "jxl")]
            {
                let texture = annotated_png_to_texture(png_bytes)?;
                let params = HdrToSdrParams {
                    input_sdr_white_nits: capture::monitor::get_sdr_white_level_nits(),
                    ..Default::default()
                };
                encode::save_jxl(&texture, path, &params, quality, output_depth)?;
                Ok(path.to_path_buf())
            }
            #[cfg(not(feature = "jxl"))]
            {
                log::warn!("JXL 编码未启用，标注结果回退保存为 PNG");
                let p = path.with_extension("png");
                std::fs::write(&p, png_bytes)?;
                Ok(p)
            }
        }
        OutputFormat::Jxr => {
            // 标注图（SDR 内容）→ scRGB 纹理 → 128bppRGBFloat JXR（SDR 内容装 HDR 容器）
            let texture = annotated_png_to_texture(png_bytes)?;
            let params = HdrToSdrParams {
                input_sdr_white_nits: capture::monitor::get_sdr_white_level_nits(),
                ..Default::default()
            };
            encode::save_jxr(&texture, path, &params, quality)?;
            Ok(path.to_path_buf())
        }
        OutputFormat::Avif => {
            #[cfg(feature = "avif")]
            {
                let img = image::load_from_memory(png_bytes)?;
                let rgba = img.to_rgba8();
                let sdr = capture::hdr_pipeline::SdrImage {
                    width: rgba.width(),
                    height: rgba.height(),
                    rgba: rgba.into_raw(),
                };
                encode::save_avif_sdr(&sdr, path)?;
                Ok(path.to_path_buf())
            }
            #[cfg(not(feature = "avif"))]
            {
                log::warn!("AVIF 编码未启用，标注结果回退保存为 PNG");
                let p = path.with_extension("png");
                std::fs::write(&p, png_bytes)?;
                Ok(p)
            }
        }
    }
}

/// 裁剪已捕获的纹理（按显示器坐标系，原点 (0,0) = monitor 左上角）
fn crop_captured_texture(
    src: &capture::CapturedTexture,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> capture::CapturedTexture {
    let bpp = src.format.bytes_per_pixel();
    let x_us = x as usize;
    let y_us = y as usize;
    let w_us = w as usize;
    let h_us = h as usize;

    let new_row_pitch = w_us * bpp;
    let mut data = vec![0u8; new_row_pitch * h_us];

    let copy_w = w_us.min(src.width.saturating_sub(x) as usize);
    let copy_h = h_us.min(src.height.saturating_sub(y) as usize);
    let copy_bytes = copy_w * bpp;

    for row in 0..copy_h {
        let src_off = (y_us + row) * src.row_pitch + x_us * bpp;
        let dst_off = row * new_row_pitch;
        data[dst_off..dst_off + copy_bytes]
            .copy_from_slice(&src.data[src_off..src_off + copy_bytes]);
    }

    capture::CapturedTexture {
        width: w,
        height: h,
        format: src.format,
        data,
        row_pitch: new_row_pitch,
        via_gdi: src.via_gdi,
    }
}

/// 进入区域选择模式（QQ 式：覆盖层零延迟显示 + 后台预截图）
///
/// 流程：
/// 1. 主窗口：置顶（钉住）保持显示（面板是用户刻意可见的浮动工具）；
///    非置顶隐藏（原行为）
/// 2. 立即显示透明覆盖层（用户马上能选择，看实时屏幕）
/// 3. 后台线程：等 250ms → 预截全屏（置顶+不含工具时临时排除主面板）
///    → 发 bg-ready 事件，前端加载为冻结背景
///
/// 预截图失败时前端回退旧的即时截图路径
#[tauri::command]
fn enter_region_select(app: tauri::AppHandle, from_panel: Option<bool>) -> Result<(), String> {
    enter_region_select_impl(&app, from_panel.unwrap_or(false)).map_err(|e| e)
}

fn enter_region_select_impl(app: &tauri::AppHandle, from_panel: bool) -> Result<(), String> {
    // 标注截图方式（配置项）：region = 拖选区域（默认）；fullscreen = 整屏快照直接标注
    let instant = Config::load().annotation_capture_mode == "fullscreen";
    log::info!(
        "进入区域选择模式（{}）",
        if instant { "整屏快照直通标注" } else { "拖选区域" }
    );

    // HDR 嵌入画布不隐藏：覆盖层置顶在其上，采集时 HDR 内容入镜（用户要截到 HDR 模式）

    // 1. 主窗口：已隐藏跳过 / 置顶保持显示 / 最小化不动 / 可见才隐藏（记录来源）
    hide_main_for_capture(app, "进入区域选择模式", from_panel);

    // 2. 立即显示区域选择覆盖层窗口（零延迟，透明看实时屏幕）
    if let Some(overlay) = app.get_webview_window("region-select") {
        log::info!("找到 region-select 窗口，准备显示");
        // 重置心跳：给前端渲染循环 8s 启动宽限（看门狗据此判断卡死）
        REGION_HEARTBEAT.store(now_secs(), Ordering::Relaxed);
        let _ = overlay.show();
        let _ = overlay.set_focus();
        let _ = overlay.set_always_on_top(true);
        let _ = app.emit("region-select://show", serde_json::json!({ "instant": instant }));
        log::info!("region-select 窗口已显示并置顶");
    } else {
        log::warn!("region-select 窗口不存在，尝试即时创建");
        match create_region_select_window(&app) {
            Ok(_) => log::info!("即时创建 region-select 窗口成功"),
            Err(e) => {
                log::error!("即时创建 region-select 窗口失败: {:#}", e);
                return Err(format!("{:#}", e));
            }
        }
    }

    // 3. 后台预截图（不阻塞覆盖层显示；覆盖层 WDA_EXCLUDEFROMCAPTURE 不会入镜）
    let app_bg = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        // 置顶 + 配置不含工具：预截图临时排除主面板
        let config = Config::load();
        let excl = exclude_main_if_needed(&app_bg, &config);
        // 整屏快照直通标注：捕获主屏全屏 → base64 → instant-annotate 事件
        if instant {
            let result = capture_fullscreen_for_annotation_inner(&app_bg, &config);
            restore_main_exclusion(excl);
            match result {
                Ok(b64) => {
                    log::info!("整屏快照→标注: {}KB", b64.len() / 1024);
                    let _ = app_bg.emit(
                        "region-select://instant-annotate",
                        serde_json::json!({ "image": b64 }),
                    );
                }
                Err(e) => {
                    log::warn!("整屏快照失败（前端回退选择模式）: {:#}", e);
                    let _ = app_bg.emit(
                        "region-select://instant-annotate",
                        serde_json::json!({ "image": "" }),
                    );
                }
            }
            return;
        }
        let result = capture_fullscreen_to_temp(&app_bg);
        restore_main_exclusion(excl);
        // HDR 画布保持显示：冻结背景含 HDR 内容（用户要截到 HDR 模式）
        match result {
            Ok(p) => {
                log::info!("预截图完成: {}", p.display());
                let _ = app_bg.emit("region-select://bg-ready", p.to_string_lossy().into_owned());
            }
            Err(e) => {
                log::warn!("预截图失败（前端回退即时截图）: {:#}", e);
                let _ = app_bg.emit("region-select://bg-ready", "");
            }
        }
    });

    Ok(())
}

/// 预截主显示器全屏为 SDR PNG 临时文件（区域选择冻结背景用）
fn capture_fullscreen_to_temp(_app: &tauri::AppHandle) -> anyhow::Result<std::path::PathBuf> {
    let config = Config::load();
    let prefer_hdr = true;
    let mut capturer = DesktopCapturer::for_monitor_primary(prefer_hdr)
        .map_err(|e| anyhow::anyhow!("创建捕获器失败: {}", e))?;
    let captured = capturer
        .capture(2)
        .map_err(|e| anyhow::anyhow!("DXGI 捕获失败: {}", e))?;

    // 统一参数入口：预设 + overrides + 显示器实际 SDR 白
    let params = config.active_tonemap_params(capture::monitor::get_sdr_white_level_nits());

    let sdr = to_sdr(&captured, &params);
    let temp_dir = std::env::temp_dir().join("jietu-hdr");
    std::fs::create_dir_all(&temp_dir)?;
    let path = temp_dir.join("region_bg.png");
    encode::save_sdr_png(&sdr, &path)?;
    Ok(path)
}

/// 全屏截图：自动捕获主显示器 → 按配置保存 → 显示预览（初版行为）
///
/// 不进标注：隐藏主窗口 → 复用静默截图管线（HDR 捕获/色调映射/保存/剪贴板）
/// → 恢复主窗口 → 发 `screenshot://captured` 事件（前端遵循 show_preview 配置弹预览）
#[tauri::command]
fn capture_fullscreen(app: tauri::AppHandle) -> Result<(), String> {
    capture_fullscreen_impl(&app, true).map_err(|e| format!("{:#}", e))
}

fn capture_fullscreen_impl(app: &tauri::AppHandle, from_panel: bool) -> anyhow::Result<()> {
    let config = Config::load();

    // HDR 嵌入画布不隐藏：采集时 HDR 内容入镜（用户要截到 HDR 模式）

    // 主窗口：已隐藏跳过 / 置顶保持显示 / 最小化不动 / 可见才隐藏（记录来源）
    hide_main_for_capture(app, "全屏截图", from_panel);
    let hidden = CAPTURE_MAIN_STATE.lock().unwrap().is_some();
    if hidden {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    // 捕获+保存+剪贴板（失败也走下方恢复路径）
    let excl = exclude_main_if_needed(app, &config);
    let result = capture_silent_inner(app, &config);
    restore_main_exclusion(excl);

    // 恢复主窗口（按截图前状态还原，不抢用户焦点）
    restore_main_after_capture(app, "全屏截图");

    let path = result?;
    // 通知前端显示预览（openPreview 遵循 show_preview 配置）
    let _ = app.emit("screenshot://captured", path);
    Ok(())
}

/// 静默截图：一键捕获主显示器并直接保存（不进标注、不弹预览）
///
/// 快速路径：HDR 捕获 → 色调映射 → 按配置格式保存到保存目录 →
/// （跟随配置）复制到剪贴板。返回保存路径（前端仅 toast 提示）。
#[tauri::command]
fn capture_silent(app: tauri::AppHandle) -> Result<String, String> {
    capture_silent_impl(&app, true).map_err(|e| format!("{:#}", e))
}

fn capture_silent_impl(app: &tauri::AppHandle, from_panel: bool) -> anyhow::Result<String> {
    let config = Config::load();

    // HDR 嵌入画布不隐藏：采集时 HDR 内容入镜（用户要截到 HDR 模式）

    // 主窗口：已隐藏跳过 / 置顶保持显示 / 最小化不动 / 可见才隐藏（记录来源）
    hide_main_for_capture(app, "静默截图", from_panel);
    let hidden = CAPTURE_MAIN_STATE.lock().unwrap().is_some();
    if hidden {
        // 等待 DWM 合成器刷新，确保窗口已从画面中移除
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    // 捕获+保存+剪贴板（失败也走下方恢复路径，避免面板消失）
    let excl = exclude_main_if_needed(app, &config);
    let result = capture_silent_inner(app, &config);
    restore_main_exclusion(excl);

    // 恢复主窗口（按截图前状态还原；toast 渲染在主窗口内，后台可见但不抢焦点）
    restore_main_after_capture(app, "静默截图");

    result
}

/// 截图成功提示音（跟随配置；MessageBeep 系统 Asterisk 柔和音）
fn play_capture_sound(config: &Config) {
    if !config.sound_enabled {
        return;
    }
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK;
    unsafe {
        let _ = MessageBeep(MB_ICONASTERISK);
    }
}

/// 鼠标所在显示器信息（用于静默截图 "cursor" 模式）
fn cursor_monitor() -> Option<capture::MonitorInfo> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut pt = POINT::default();
    unsafe {
        if GetCursorPos(&mut pt).is_err() {
            return None;
        }
    }
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let monitors = capture::monitor::enumerate_monitors().ok()?;
    monitors.into_iter().find(|m| m.hmonitor == hmon.0 as isize)
}

/// 静默截图参数记录（伴随图片生成的 .txt 文档）
///
/// 记录本次截图的完整参数链：时间/输出格式/捕获方式/像素格式/显示器状态/
/// 色调映射参数——HDR→SDR 出图问题可凭此文件离线复现与追溯。
#[allow(clippy::too_many_arguments)]
fn write_capture_params_sidecar(
    image_path: &std::path::Path,
    config: &Config,
    params: &HdrToSdrParams,
    captured: &capture::CapturedTexture,
    monitor: &capture::monitor::MonitorInfo,
    output_format: OutputFormat,
) {
    let fmt_name = match output_format {
        OutputFormat::PngSdr => "SDR PNG (8bit sRGB)",
        OutputFormat::PngHdr => "HDR PNG (16bit PQ BT.2020 + cICP)",
        OutputFormat::Exr => "OpenEXR (32bit 浮点 scRGB)",
        OutputFormat::Jxl => "HDR JPEG XL (16bit PQ BT.2020)",
        OutputFormat::Jxr => "HDR JPEG XR (128bppRGBFloat scRGB)",
        OutputFormat::Avif => "AVIF",
    };
    let pix_fmt = match captured.format {
        color::PixelFormat::R16g16b16a16Float => "scRGB FP16 线性光 (BT.709 原色)",
        color::PixelFormat::R10g10b10a2 => "HDR10 R10G10B10A2 (PQ / BT.2020)",
        color::PixelFormat::Bgra8 => "BGRA8 SDR",
    };
    let capture_method = if captured.via_gdi {
        "GDI BitBlt (DDA 黑帧兜底)"
    } else {
        "DXGI Desktop Duplication"
    };
    let preset_name = {
        let p = config.tonemap_settings.resolve_active_preset();
        let ov_note = if config.tonemap_settings.overrides.is_empty() {
            String::new()
        } else {
            "（含高级调整）".to_string()
        };
        format!("{}{}", p.name, ov_note)
    };
    let mapping_note = match output_format {
        OutputFormat::Exr => "注: EXR 保存原始 scRGB 浮点数据，未应用色调映射（以下为当时配置）",
        OutputFormat::PngHdr => {
            "注: HDR PNG 保存 PQ 绝对亮度数据（scRGB 1.0 = 80 nits 物理基准），未应用色调映射"
        }
        OutputFormat::Jxl => {
            "注: HDR JXL 保存 16bit PQ 绝对亮度数据（scRGB 1.0 = 80 nits 物理基准），未应用色调映射"
        }
        OutputFormat::Jxr => {
            "注: HDR JXR 保存 128bppRGBFloat scRGB 数据（1.0 = 80 nits 物理基准），未应用色调映射"
        }
        _ => "注: 以下参数已实际应用于本次 SDR 输出",
    };
    let content = format!(
        "jietu-hdr 截图参数记录\n\
         ================================\n\
         截图时间: {}\n\
         输出文件: {}\n\
         输出格式: {}\n\
         \n\
         [捕获]\n\
         捕获方式: {}\n\
         像素格式: {}\n\
         图像尺寸: {}x{} (物理像素)\n\
         \n\
         [显示器]\n\
         设备: {} ({})\n\
         分辨率: {}x{}\n\
         HDR 模式: {:?}\n\
         位深: {} bit/通道\n\
         最大全帧亮度: {} nits\n\
         \n\
         [HDR→SDR 色调映射]\n\
         {}\n\
         激活预设: {}\n\
         算子: {:?}\n\
         场景峰值亮度: {} nits{}\n\
         输入 SDR 白: {} nits (显示器实际读数)\n\
         输出 diffuse white: {}\n\
         曝光: {} EV\n\
         饱和度补偿: {}\n\
         对比度: {}\n\
         色域压缩强度: {}\n\
         量化抖动: {}\n\
         \n\
         [配置快照]\n\
         目标显示器策略: {}\n\
         复制到剪贴板: {}\n\
         自动保存: {}\n\
         提示音: {}\n\
         \n\
         由 jietu-hdr v{} 生成\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        image_path.display(),
        fmt_name,
        capture_method,
        pix_fmt,
        captured.width,
        captured.height,
        monitor.device_name,
        monitor.adapter_name,
        monitor.width,
        monitor.height,
        monitor.hdr_mode,
        monitor.bits_per_color,
        monitor.max_full_frame_luminance,
        mapping_note,
        preset_name,
        params.operator,
        params.source_peak_nits,
        if params.adaptive_peak {
            "（智能自适应）"
        } else {
            ""
        },
        params.input_sdr_white_nits,
        params.output_diffuse_white,
        params.exposure_ev,
        params.saturation,
        params.contrast,
        params.gamut_strength,
        params.dither,
        config.silent_monitor,
        config.copy_to_clipboard,
        config.auto_save,
        config.sound_enabled,
        env!("CARGO_PKG_VERSION"),
    );
    let sidecar = image_path.with_extension("txt");
    match std::fs::write(&sidecar, content) {
        Ok(_) => log::info!("参数记录已保存: {}", sidecar.display()),
        Err(e) => log::warn!("参数记录写入失败: {}", e),
    }
}

fn capture_silent_inner(app: &tauri::AppHandle, config: &Config) -> anyhow::Result<String> {
    // 目标显示器：主显示器 / 鼠标所在显示器（跟随配置）
    let prefer_hdr = true;
    let mut capturer = if config.silent_monitor == "cursor" {
        match cursor_monitor() {
            Some(m) => {
                log::info!(
                    "静默截图目标: 鼠标所在显示器 {} ({})",
                    m.device_name,
                    m.adapter_name
                );
                DesktopCapturer::for_monitor(&m, prefer_hdr)
            }
            None => {
                log::warn!("无法定位鼠标显示器，回退主显示器");
                DesktopCapturer::for_monitor_primary(prefer_hdr)
            }
        }
    } else {
        DesktopCapturer::for_monitor_primary(prefer_hdr)
    }
    .map_err(|e| anyhow::anyhow!("创建捕获器失败: {}", e))?;
    let captured = capturer
        .capture(2)
        .map_err(|e| anyhow::anyhow!("DXGI 捕获失败: {}", e))?;
    log::info!(
        "静默截图捕获: {}x{} format={:?}",
        captured.width,
        captured.height,
        captured.format
    );

    // 统一参数入口：预设 + overrides + 显示器实际 SDR 白
    let params = config.active_tonemap_params(capture::monitor::get_sdr_white_level_nits());

    // 保存到配置目录
    let save_dir = config.resolved_save_dir();
    std::fs::create_dir_all(&save_dir)?;
    let filename = config.generate_filename();

    let path = match config.output_format {
        OutputFormat::PngHdr => {
            // HDR PNG：PQ 绝对亮度编码（编码器内部 scRGB×80→nits，与 SDR 白无关）
            let p = save_dir.join(format!("{}.png", filename));
            encode::save_hdr_png(&captured, &p, &params)?;
            p
        }
        OutputFormat::Jxl => {
            // HDR JXL：16bit PQ BT.2020（绝对亮度，编码器内部 scRGB×80→nits）
            #[cfg(feature = "jxl")]
            {
                let p = save_dir.join(format!("{}.jxl", filename));
                encode::save_jxl(
                    &captured,
                    &p,
                    &params,
                    config.output_quality,
                    config.output_depth,
                )?;
                p
            }
            #[cfg(not(feature = "jxl"))]
            {
                let p = save_dir.join(format!("{}.png", filename));
                let sdr = to_sdr(&captured, &params);
                encode::save_sdr_png(&sdr, &p)?;
                p
            }
        }
        OutputFormat::Jxr => {
            // HDR JXR：128bppRGBFloat，scRGB 直存（1.0=80 nits 物理语义）
            let p = save_dir.join(format!("{}.jxr", filename));
            encode::save_jxr(&captured, &p, &params, config.output_quality)?;
            p
        }
        OutputFormat::Exr => {
            let p = save_dir.join(format!("{}.exr", filename));
            encode::save_exr(&captured, &p)?;
            p
        }
        // PngSdr；Avif 未启用 feature 时同样回退 SDR PNG
        _ => {
            let p = save_dir.join(format!("{}.png", filename));
            let sdr = to_sdr(&captured, &params);
            encode::save_sdr_png(&sdr, &p)?;
            p
        }
    };

    // 参数记录：伴随图片生成同名 .txt（追溯 HDR→SDR 出图参数链）
    write_capture_params_sidecar(
        &path,
        config,
        &params,
        &captured,
        &capturer.monitor,
        config.output_format,
    );

    // 跟随配置复制到剪贴板（带重试）
    if config.copy_to_clipboard {
        if let Ok(bytes) = std::fs::read(&path) {
            match tauri::image::Image::from_bytes(&bytes) {
                Ok(img) => {
                    if let Err(e) = viewer::write_clipboard_image_with_retry(&app, &img) {
                        log::warn!("静默截图：{}", e);
                    }
                }
                Err(e) => log::warn!("静默截图：构造剪贴板图像失败: {}", e),
            }
        }
    }

    log::info!("静默截图已保存: {}", path.display());
    // 截图 AI 增强（增值项：后台生成 2x 增强副本，不替换原图）
    if config.upscale_after_capture {
        upscale::commands::enhance_capture_background(&path);
    }
    play_capture_sound(config);
    Ok(path.to_string_lossy().into_owned())
}

/// 取消区域选择模式（ESC 或用户主动取消 / 复制保存完成收尾）
///
/// 主窗口恢复统一走状态机（按发起来源）：
/// - 面板发起 → 还原并聚焦；热键/托盘发起 → 保持最小化不抢焦点
/// - show_main=true（复制/保存/贴图完成收尾）：commit_annotation /
///   pin_screenshot 可能已提前消费状态并完成恢复（此处 no-op 双保险）；
///   OCR 复制等未经过后端命令的路径，状态仍在 → 按来源恢复
/// - show_main=false（ESC 取消等）：同状态机（不抢焦点）
#[tauri::command]
fn cancel_region_select(app: tauri::AppHandle, show_main: bool) -> Result<(), String> {
    log::info!("取消区域选择模式：开始 (show_main={})", show_main);

    // 隐藏覆盖层窗口
    if let Some(overlay) = app.get_webview_window("region-select") {
        match overlay.hide() {
            Ok(_) => log::info!("取消区域选择模式：region-select 已隐藏"),
            Err(e) => log::error!("取消区域选择模式：隐藏 region-select 失败: {}", e),
        }
    } else {
        log::warn!("取消区域选择模式：未找到 region-select 窗口");
    }

    restore_main_after_capture(
        &app,
        if show_main {
            "区域选择完成"
        } else {
            "取消区域选择模式"
        },
    );

    // 丢弃未消费的区域 HDR 纹理缓存（防陈旧数据流入下一次会话）
    let _ = take_region_hdr_texture();

    log::info!("取消区域选择模式：完成");
    Ok(())
}

/// 截取指定区域（物理像素坐标，相对于主显示器左上角）
///
/// 用户在透明覆盖层上完成选择后调用：
/// 1. 隐藏覆盖层窗口
/// 2. 用 DXGI 捕获主显示器
/// 3. 裁剪到用户选择的区域
/// 4. 色调映射转 SDR，保存到临时文件
/// 5. 发送 annotation://start 事件给主窗口
/// 6. 显示主窗口并最大化（进入标注模式）
#[tauri::command]
fn capture_region_at(
    app: tauri::AppHandle,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<String, String> {
    capture_region_at_impl(&app, x, y, w, h).map_err(|e| format!("{:#}", e))
}

fn capture_region_at_impl(
    app: &tauri::AppHandle,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> anyhow::Result<String> {
    log::info!("截取区域: x={} y={} w={} h={}", x, y, w, h);

    // 1. 隐藏覆盖层窗口（避免遮挡截图）
    if let Some(overlay) = app.get_webview_window("region-select") {
        let _ = overlay.hide();
    }
    // 等待一帧让窗口真正隐藏
    std::thread::sleep(std::time::Duration::from_millis(50));

    let config = Config::load();

    // 2. 捕获整个主显示器
    let prefer_hdr = true;
    let mut capturer = DesktopCapturer::for_monitor_primary(prefer_hdr)
        .map_err(|e| anyhow::anyhow!("创建捕获器失败: {}", e))?;
    let captured = capturer
        .capture(2)
        .map_err(|e| anyhow::anyhow!("DXGI 捕获失败: {}", e))?;

    let sdr_white_nits = capture::monitor::get_sdr_white_level_nits();
    let params = config.active_tonemap_params(sdr_white_nits);

    // 3. 裁剪到用户选择的区域
    let cropped = crop_captured_texture(&captured, x, y, w, h);

    // 4. 色调映射转 SDR 并保存到临时文件
    let temp_dir = std::env::temp_dir().join("jietu-hdr");
    std::fs::create_dir_all(&temp_dir)?;
    let temp_path = temp_dir.join("region_capture.png");
    let sdr = to_sdr(&cropped, &params);
    encode::save_sdr_png(&sdr, &temp_path)?;

    log::info!("区域截图临时文件: {}", temp_path.display());

    // 5. 发送事件给主窗口，前端切换到标注视图
    let path_str = temp_path.to_string_lossy().into_owned();
    let _ = app.emit("annotation://start", &path_str);

    // 6. 显示主窗口并最大化（进入标注模式）
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.maximize();
        let _ = main.set_focus();
    }

    Ok(path_str)
}

/// 截取指定区域并复制到剪贴板（✅️ 按钮）
///
/// 用户在编辑菜单点击✅️后调用：
/// 1. 隐藏覆盖层窗口
/// 2. 用 DXGI 捕获主显示器并裁剪到指定区域
/// 3. 色调映射转 SDR PNG
/// 4. 写入系统剪贴板
/// 5. 关闭覆盖层，恢复主窗口
#[tauri::command]
fn capture_region_to_clipboard(
    app: tauri::AppHandle,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(), String> {
    log::info!("区域截图→剪贴板: x={} y={} w={} h={}", x, y, w, h);

    // 1. 隐藏覆盖层窗口
    if let Some(overlay) = app.get_webview_window("region-select") {
        let _ = overlay.hide();
    }
    std::thread::sleep(std::time::Duration::from_millis(50));

    let config = Config::load();

    // 2. 捕获并裁剪（置顶 + 配置不含工具：临时排除主面板）
    let excl = exclude_main_if_needed(&app, &config);
    let png_data = capture_region_to_png(&app, &config, x, y, w, h);
    restore_main_exclusion(excl);
    let png_data = png_data?;

    // 3. 写入剪贴板（带重试：OpenClipboard 瞬态占用常态）
    {
        match tauri::image::Image::from_bytes(&png_data) {
            Ok(img) => {
                if let Err(e) = viewer::write_clipboard_image_with_retry(&app, &img) {
                    log::error!("{}", e);
                    return Err(e);
                }
            }
            Err(e) => {
                let msg = format!("构造剪贴板图像失败: {}", e);
                log::error!("{}", msg);
                return Err(msg);
            }
        }
    }
    log::info!("已复制到剪贴板");

    // 4. 恢复主窗口（按截图前状态还原，不抢用户焦点）
    restore_main_after_capture(&app, "区域截图→剪贴板");

    Ok(())
}

/// 截取指定区域并另存为（弹出保存对话框）
///
/// 用户在编辑菜单点击「另存为」后调用：
/// 1. 隐藏覆盖层窗口
/// 2. 用 DXGI 捕获主显示器并裁剪到指定区域
/// 3. 色调映射转 SDR PNG
/// 4. 弹出保存对话框让用户选择路径
/// 5. 写入文件
/// 6. 关闭覆盖层，恢复主窗口
#[tauri::command]
async fn capture_region_save_as(
    app: tauri::AppHandle,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(), String> {
    log::info!("区域截图→另存为: x={} y={} w={} h={}", x, y, w, h);

    // 1. 隐藏覆盖层窗口
    if let Some(overlay) = app.get_webview_window("region-select") {
        let _ = overlay.hide();
    }
    std::thread::sleep(std::time::Duration::from_millis(50));

    let config = Config::load();

    // 2. 捕获并裁剪（置顶 + 配置不含工具：临时排除主面板）
    let excl = exclude_main_if_needed(&app, &config);
    let png_data = capture_region_to_png(&app, &config, x, y, w, h);
    restore_main_exclusion(excl);
    let png_data = png_data?;

    // 3. 弹出保存对话框
    use tauri_plugin_dialog::DialogExt;
    let save_path = app
        .dialog()
        .file()
        .add_filter("PNG 图片", &["png"])
        .set_file_name(&format!(
            "screenshot_{}.png",
            chrono::Local::now().format("%Y%m%d_%H%M%S")
        ))
        .blocking_save_file()
        .ok_or_else(|| "未选择保存路径".to_string())?;

    // FilePath → PathBuf
    let save_path = save_path
        .into_path()
        .map_err(|e| format!("路径解析失败: {}", e))?;

    log::info!("保存到: {}", save_path.display());

    // 4. 写入文件
    std::fs::write(&save_path, &png_data).map_err(|e| format!("写入文件失败: {}", e))?;
    log::info!("已保存");

    // 5. 恢复主窗口（按截图前状态还原，不抢用户焦点）
    restore_main_after_capture(&app, "区域截图→另存为");

    Ok(())
}

/// 获取 region-select 覆盖层的捕获基准
///
/// 返回 (覆盖层所在显示器, 覆盖层客户区原点相对该显示器原点的物理偏移)。
/// 前端传来的区域坐标（selection * dpr）是相对覆盖层客户区的物理坐标，
/// 加上偏移后即得到相对捕获显示器原点的坐标——整条链路以覆盖层客户区
/// 为锚点，与窗口识别（MapWindowPoints）保持同一基准，消除吸附偏移。
fn overlay_capture_origin(
    app: &tauri::AppHandle,
) -> Result<(capture::MonitorInfo, i32, i32), String> {
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::{
        ClientToScreen, MonitorFromWindow, MONITOR_DEFAULTTONEAREST,
    };

    let overlay = app
        .get_webview_window("region-select")
        .ok_or_else(|| "region-select 窗口不存在".to_string())?;
    let hwnd = HWND(
        overlay
            .hwnd()
            .map_err(|e| format!("获取覆盖层句柄失败: {}", e))?
            .0 as *mut _,
    );

    // 客户区 (0,0) 的屏幕坐标（hide 后窗口位置不变，仍可查询）
    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        ClientToScreen(hwnd, &mut pt)
            .ok()
            .map_err(|e| format!("ClientToScreen 失败: {}", e))?;
    }

    // 覆盖层所在显示器
    let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let monitors =
        capture::monitor::enumerate_monitors().map_err(|e| format!("枚举显示器失败: {}", e))?;
    let monitor = monitors
        .iter()
        .find(|m| m.hmonitor == hmon.0 as isize)
        .or_else(|| monitors.first())
        .cloned()
        .ok_or_else(|| "未找到显示器".to_string())?;

    let dx = pt.x - monitor.left;
    let dy = pt.y - monitor.top;
    log::info!(
        "覆盖层锚点: 客户区原点=({},{}) 显示器={} 原点=({},{}) 偏移=({},{})",
        pt.x,
        pt.y,
        monitor.device_name,
        monitor.left,
        monitor.top,
        dx,
        dy
    );
    Ok((monitor, dx, dy))
}

/// 区域截图 HDR 纹理缓存（确认选区时存，标注保存时消费）
///
/// 区域截图链路的 HDR 保留：`capture_region_for_annotation` 裁剪后的
/// scRGB 纹理存此；用户**无标注**保存时直接按配置格式（JXL/JXR/PngHdr/EXR）
/// 从该纹理编码——真 HDR 截图（画布高光真实 nits 保留）。画过标注则
/// 走原 SDR canvas 路径（标注层是 2D 绘制，HDR 合成无意义）。
static REGION_HDR_TEXTURE: std::sync::OnceLock<std::sync::Mutex<Option<capture::CapturedTexture>>> =
    std::sync::OnceLock::new();

fn region_hdr_cache() -> &'static std::sync::Mutex<Option<capture::CapturedTexture>> {
    REGION_HDR_TEXTURE.get_or_init(|| std::sync::Mutex::new(None))
}

/// 取走缓存的区域 HDR 纹理（一次性消费；无则 None）
fn take_region_hdr_texture() -> Option<capture::CapturedTexture> {
    region_hdr_cache().lock().ok().and_then(|mut g| g.take())
}

/// 内部辅助：截取指定区域并转为 PNG 字节数据（同时缓存裁剪后的 HDR 纹理）
///
/// `x/y/w/h` 为相对覆盖层客户区的物理像素坐标（前端 selection * dpr）。
fn capture_region_to_png(
    app: &tauri::AppHandle,
    config: &Config,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>, String> {
    // 覆盖层所在显示器 + 客户区原点偏移（消除边框溢出/多显示器偏移）
    let (monitor, dx, dy) = overlay_capture_origin(app)?;

    // 捕获覆盖层所在显示器（而非固定主显示器）
    let prefer_hdr = true;
    let mut capturer = DesktopCapturer::for_monitor(&monitor, prefer_hdr)
        .map_err(|e| format!("创建捕获器失败: {}", e))?;
    let captured = capturer
        .capture(2)
        .map_err(|e| format!("DXGI 捕获失败: {}", e))?;

    // 统一参数入口：预设 + overrides + 显示器实际 SDR 白
    let params = config.active_tonemap_params(capture::monitor::get_sdr_white_level_nits());

    // 覆盖层客户区坐标 → 显示器坐标，并夹紧到捕获范围内
    let crop_x = ((x as i32 + dx).max(0) as u32).min(captured.width.saturating_sub(1));
    let crop_y = ((y as i32 + dy).max(0) as u32).min(captured.height.saturating_sub(1));
    let crop_w = w.min(captured.width - crop_x);
    let crop_h = h.min(captured.height - crop_y);
    log::info!(
        "区域裁剪: overlay=({},{},{}x{}) → 显示器坐标=({},{},{}x{})",
        x,
        y,
        w,
        h,
        crop_x,
        crop_y,
        crop_w,
        crop_h
    );

    // 裁剪到指定区域
    let cropped = crop_captured_texture(&captured, crop_x, crop_y, crop_w, crop_h);

    // 缓存 HDR 纹理（无标注保存时直存真 HDR；消费点 commit/save_annotation_as）
    if cropped.format == crate::color::PixelFormat::R16g16b16a16Float {
        if let Ok(mut g) = region_hdr_cache().lock() {
            *g = Some(cropped.clone());
        }
        log::info!(
            "区域 HDR 纹理已缓存: {}x{} scRGB（无标注保存时直存 HDR）",
            crop_w,
            crop_h
        );
    }

    // 色调映射转 SDR
    let sdr = to_sdr(&cropped, &params);

    // 编码为 PNG 字节数据（内存中）
    encode::encode_sdr_png_bytes(&sdr).map_err(|e| format!("编码 PNG 失败: {}", e))
}

/// 整屏快照→标注（annotation_capture_mode="fullscreen"）：
/// 捕获主显示器全屏（DXGI 确认瞬间帧）→ 缓存完整 HDR 纹理 → to_sdr → PNG 字节
/// 调用方需已用 exclude_main_if_needed / restore_main_exclusion 包裹（与区域路径同款）
fn capture_fullscreen_for_annotation_inner(
    _app: &tauri::AppHandle,
    config: &Config,
) -> Result<String, String> {
    let prefer_hdr = true;
    let mut capturer = DesktopCapturer::for_monitor_primary(prefer_hdr)
        .map_err(|e| format!("创建捕获器失败: {}", e))?;
    let captured = capturer
        .capture(2)
        .map_err(|e| format!("DXGI 捕获失败: {}", e))?;

    // 完整纹理入 HDR 缓存（无标注保存时直存真 HDR，与区域路径语义一致）
    if captured.format == crate::color::PixelFormat::R16g16b16a16Float {
        if let Ok(mut g) = region_hdr_cache().lock() {
            *g = Some(captured.clone());
        }
        log::info!(
            "整屏 HDR 纹理已缓存: {}x{} scRGB（无标注保存时直存 HDR）",
            captured.width,
            captured.height
        );
    }

    // 统一参数入口：预设 + overrides + 显示器实际 SDR 白
    let params = config.active_tonemap_params(capture::monitor::get_sdr_white_level_nits());
    let sdr = to_sdr(&captured, &params);
    let png_data =
        encode::encode_sdr_png_bytes(&sdr).map_err(|e| format!("编码 PNG 失败: {}", e))?;

    // OCR 临时文件异步落盘（固定路径，前端 ensureOcrPath 轮询依赖）
    write_annotation_temp_file_async(png_data.clone());

    // base64 直传前端（同步零文件 IO）
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&png_data))
}

/// 标注截图 OCR 临时文件异步落盘（写失败仅记日志，OCR 届时轮询最多等 2s）
fn write_annotation_temp_file_async(png_data: Vec<u8>) {
    std::thread::spawn(move || {
        let temp_dir = std::env::temp_dir().join("jietu-hdr");
        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            log::warn!("异步写临时目录失败（OCR 可能不可用）: {}", e);
            return;
        }
        let temp_path = temp_dir.join("region_annotation.png");
        match std::fs::write(&temp_path, &png_data) {
            Ok(_) => log::info!("标注临时文件已异步写入: {}", temp_path.display()),
            Err(e) => log::warn!("异步写标注临时文件失败（OCR 可能不可用）: {}", e),
        }
    });
}

/// 截取指定区域并直接返回 base64 PNG（内存直传，不落盘等待）
///
/// 用户在透明覆盖层选区确定后调用：实时截取指定区域
///
/// 覆盖层已设置 WDA_EXCLUDEFROMCAPTURE（不参与屏幕捕获），
/// 因此**无需隐藏窗口**直接截屏，遮罩/虚线框/放大镜均不会入镜。
/// 截取确认瞬间的实时画面（游戏等动态内容与所见一致）。
///
/// 性能设计：
/// 1. PNG 数据 base64 后直接随命令返回，前端零文件 IO 即可上 canvas
/// 2. OCR 用临时文件**异步**落盘（spawn 线程），不阻塞标注模式显示
#[tauri::command]
fn capture_region_for_annotation(
    app: tauri::AppHandle,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<String, String> {
    log::info!("区域截图→标注(实时): x={} y={} w={} h={}", x, y, w, h);

    let config = Config::load();

    // 截取并裁剪（DXGI 拿当前帧 = 确认瞬间实时画面）
    // 置顶 + 配置不含工具：临时排除主面板
    let excl = exclude_main_if_needed(&app, &config);
    let png_data = capture_region_to_png(&app, &config, x, y, w, h);
    restore_main_exclusion(excl);
    let png_data = png_data?;

    // OCR 临时文件异步落盘（不阻塞返回；写失败仅记日志，OCR 届时不可用）
    write_annotation_temp_file_async(png_data.clone());

    // base64 直传前端（同步零文件 IO）
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_data);
    log::info!(
        "实时截图 base64 直传: {}KB → {}",
        png_data.len() / 1024,
        "前端"
    );

    Ok(b64)
}

/// 即时创建区域选择覆盖层窗口（备用：配置文件未预定义时使用）
fn create_region_select_window(app: &tauri::AppHandle) -> anyhow::Result<()> {
    let window = WebviewWindowBuilder::new(
        app,
        "region-select",
        tauri::WebviewUrl::App("#/region-select".into()),
    )
    .title("")
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .resizable(false)
    .fullscreen(true)
    .visible(true)
    .build()
    .map_err(|e| anyhow::anyhow!("创建区域选择窗口失败: {}", e))?;

    // 设置 WDA_EXCLUDEFROMCAPTURE：窗口不参与屏幕捕获
    let hwnd = window
        .hwnd()
        .map_err(|e| anyhow::anyhow!("获取窗口句柄失败: {}", e))?;
    set_window_display_affinity(hwnd.0 as isize, true);
    capture::dxgi_duplication::set_region_select_hwnd(hwnd.0 as isize);

    Ok(())
}

/// 进入标注模式（不创建新窗口，复用主窗口切换视图）
///
/// 关键设计：
/// - **不创建新的 webview 窗口**：新窗口需要重新加载 Vue 应用，
///   dev 模式下需要 2-5 秒，期间窗口拦截所有输入但白屏，导致鼠标卡死只能重启。
/// - **复用主窗口**：主窗口已加载 Vue 应用，通过事件切换视图，即时响应。
/// - **最大化主窗口**：提供全屏标注体验。
///
/// `image_path`: 截图临时文件路径
#[tauri::command]
fn open_annotation_window(app: tauri::AppHandle, image_path: String) -> Result<(), String> {
    log::info!("进入标注模式，图像路径: {}", image_path);

    // 发送事件给主窗口，前端切换到标注视图
    app.emit("annotation://start", &image_path)
        .map_err(|e| format!("发送事件失败: {}", e))?;

    // 最大化主窗口（先 show，因为 capture_region 时已 hide）
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.maximize();
        let _ = main.set_focus();
    }

    Ok(())
}

/// 退出标注模式，恢复主窗口
#[tauri::command]
fn close_annotation_window(app: tauri::AppHandle) -> Result<(), String> {
    log::info!("退出标注模式");

    // 发送事件给主窗口，前端切换回主视图
    app.emit("annotation://done", ())
        .map_err(|e| format!("发送事件失败: {}", e))?;

    // 恢复主窗口
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.unmaximize();
        let _ = main.show();
        let _ = main.set_focus();
    }

    Ok(())
}

/// 提交标注结果：将前端导出的 PNG 数据保存到最终保存目录
///
/// - `png_data`: base64 编码的 PNG 数据（不含 data: 前缀）
/// - `auto_save`: 是否自动保存到磁盘
/// - `has_annotations`: 用户是否画过标注（false = 无标注直接保存——
///   若 HDR 纹理缓存可用则直存真 HDR，跳过 SDR canvas 往返）
/// 返回最终保存路径（auto_save=true）或空字符串
#[tauri::command]
fn commit_annotation(
    app: tauri::AppHandle,
    png_data: String,
    auto_save: bool,
    copy_to_clipboard: bool,
    has_annotations: Option<bool>,
) -> Result<String, String> {
    log::info!("commit_annotation 被调用: auto_save={} copy_to_clipboard={} has_annotations={:?} png_len={}", auto_save, copy_to_clipboard, has_annotations, png_data.len());
    commit_annotation_impl(
        &app,
        &png_data,
        auto_save,
        copy_to_clipboard,
        has_annotations.unwrap_or(true),
    )
    .map_err(|e| {
        log::error!("commit_annotation 失败: {:#}", e);
        format!("{:#}", e)
    })
}

/// 按文件路径复制图片到剪贴板（预览弹窗"复制"按钮用；动图走 CFHDROP 保留动画）
#[tauri::command]
fn copy_image_file_to_clipboard(app: tauri::AppHandle, path: String) -> Result<(), String> {
    viewer::copy_image_to_clipboard_impl(&app, &path)
}

fn commit_annotation_impl(
    app: &tauri::AppHandle,
    png_data: &str,
    auto_save: bool,
    copy_to_clipboard: bool,
    has_annotations: bool,
) -> anyhow::Result<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_data.trim())
        .map_err(|e| anyhow::anyhow!("base64 解码失败: {}", e))?;

    // 输出格式跟随配置（区域截图与全屏截图一致）
    let config = Config::load();
    let output_format = config.output_format;
    let output_quality = config.output_quality;

    // 无标注 + HDR 纹理缓存命中 → 直存真 HDR（区域截图的 HDR 保留路径；
    // 剪贴板仍用 SDR PNG bytes）。缓存一次性消费（无论走哪条路径都取走防陈旧）
    let hdr_texture = if has_annotations {
        take_region_hdr_texture(); // 丢弃（画了标注，HDR 路径不适用）
        None
    } else {
        take_region_hdr_texture()
    };

    let final_path = if auto_save {
        let save_dir = config.resolved_save_dir();
        std::fs::create_dir_all(&save_dir)?;
        let filename = config.generate_filename();
        let path = save_dir.join(format!("{}.{}", filename, output_format.extension()));
        let actual = match &hdr_texture {
            Some(tex) => {
                // 真 HDR 直存：scRGB 纹理按配置格式编码（高光真实 nits 保留）
                let params =
                    config.active_tonemap_params(capture::monitor::get_sdr_white_level_nits());
                save_captured_as_format(
                    tex,
                    output_format,
                    &path,
                    &params,
                    output_quality,
                    config.output_depth,
                )?
            }
            None => save_annotated_as_format(
                &bytes,
                output_format,
                &path,
                output_quality,
                config.output_depth,
            )?,
        };
        log::info!(
            "标注截图已保存: {} (format={:?} hdr_direct={})",
            actual.display(),
            output_format,
            hdr_texture.is_some()
        );
        // 截图 AI 增强（增值项：后台生成 2x 增强副本，不替换原图）
        if config.upscale_after_capture {
            upscale::commands::enhance_capture_background(&actual);
        }
        actual.to_string_lossy().into_owned()
    } else {
        String::new()
    };

    // 复制到剪贴板（带重试；失败如实上报前端，不再伪装成功）
    let mut copy_error: Option<String> = None;
    if copy_to_clipboard {
        match tauri::image::Image::from_bytes(&bytes) {
            Ok(img) => {
                if let Err(e) = viewer::write_clipboard_image_with_retry(app, &img) {
                    log::warn!("写入剪贴板失败: {}", e);
                    copy_error = Some(e);
                }
            }
            Err(e) => {
                log::warn!("构造剪贴板图像失败: {}", e);
                copy_error = Some(format!("构造剪贴板图像失败: {}", e));
            }
        }
    }

    // 退出标注模式：发送事件 + 取消最大化；
    // 主窗口按截图前状态恢复（隐藏前有焦点才重新聚焦——后台/托盘态不弹前台）
    let _ = app.emit("annotation://done", ());
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unmaximize();
    }
    restore_main_after_capture(app, "commit_annotation");

    // 轻量反馈（不弹预览）：保存 → toast 文件名；复制 → toast 确认
    if !final_path.is_empty() {
        let _ = app.emit("screenshot://saved", &final_path);
    }
    if copy_to_clipboard {
        // 复制失败 → 上报错误事件（前端 toast 如实提示）；成功才发 copied
        match &copy_error {
            Some(err) => {
                let _ = app.emit("screenshot://copy-failed", err.clone());
            }
            None => {
                let _ = app.emit("screenshot://copied", ());
            }
        }
    }
    // 保存或复制成功 → 提示音
    if !final_path.is_empty() || (copy_to_clipboard && copy_error.is_none()) {
        play_capture_sound(&config);
    }
    Ok(final_path)
}

/// 标注结果另存为：将前端导出的 PNG 数据按配置格式转换后写入用户选择的路径
///
/// - `png_data`: base64 编码的 PNG 数据（不含 data: 前缀）
/// - `path`: 用户在保存对话框中选择的完整路径
/// - `has_annotations`: 是否画过标注（无标注 + HDR 缓存命中 → 直存真 HDR）
/// 返回实际写入的路径（JXL/AVIF 未启用回退 PNG 时扩展名会变化）
#[tauri::command]
fn save_annotation_as(
    app: tauri::AppHandle,
    png_data: String,
    path: String,
    has_annotations: Option<bool>,
) -> Result<String, String> {
    log::info!("save_annotation_as: {}", path);
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_data.trim())
        .map_err(|e| format!("base64 解码失败: {}", e))?;

    let config = Config::load();
    let output_format = config.output_format;
    let target = std::path::PathBuf::from(&path);

    // 无标注 + HDR 纹理缓存命中 → 直存真 HDR（与 commit_annotation 同语义）
    let hdr_texture = if has_annotations.unwrap_or(true) {
        take_region_hdr_texture();
        None
    } else {
        take_region_hdr_texture()
    };
    let actual = match &hdr_texture {
        Some(tex) => {
            let params = config.active_tonemap_params(capture::monitor::get_sdr_white_level_nits());
            save_captured_as_format(
                tex,
                output_format,
                &target,
                &params,
                config.output_quality,
                config.output_depth,
            )
        }
        None => save_annotated_as_format(
            &bytes,
            output_format,
            &target,
            config.output_quality,
            config.output_depth,
        ),
    }
    .map_err(|e| {
        log::error!("save_annotation_as 失败: {:#}", e);
        format!("{:#}", e)
    })?;

    // 退出标注模式：发送事件 + 恢复主窗口
    let _ = app.emit("annotation://done", ());
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
    let result = actual.to_string_lossy().into_owned();
    let _ = app.emit("screenshot://captured", &result);
    play_capture_sound(&config);
    Ok(result)
}

/// 打开贴图窗口
///
/// - `image_path`: 要贴在屏幕上的图像路径
/// - `x`, `y`: 贴图初始位置（屏幕坐标）
/// - `w`, `h`: 贴图初始尺寸
#[tauri::command]
async fn open_pin_window(
    app: tauri::AppHandle,
    image_path: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), String> {
    log::info!(
        "open_pin_window 开始: path={} pos=({},{}) size={}x{}",
        image_path,
        x,
        y,
        w,
        h
    );

    let label = format!("pin-{}", next_pin_id(&app));
    log::info!("open_pin_window label={}", label);

    // 使用 App 内路由（dev 和 release 都可用，不依赖外部 HTTP 服务）
    let path = format!("/#/pin?img={}&w={}&h={}", url_encode(&image_path), w, h);
    log::info!("open_pin_window 路径={}", path);
    log::logger().flush();

    let window = WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(path.into()))
        .title("")
        .position(x, y)
        .inner_size(w, h)
        .decorations(false)
        .transparent(true)
        .shadow(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(true)
        .min_inner_size(80.0, 80.0)
        .build()
        .map_err(|e| {
            log::error!("open_pin_window build 失败: {}", e);
            log::logger().flush();
            format!("创建贴图窗口失败: {}", e)
        })?;

    log::info!("open_pin_window 窗口已创建，准备显示");
    let _ = window.show();
    let _ = window.set_focus();
    log::info!("open_pin_window 完成");
    Ok(())
}

/// 贴图组合命令：一次性完成"隐藏区域选择窗口 + 创建贴图窗口 + 显示主窗口"
///
/// 避免前端多次异步 invoke 导致的窗口 z-order 卡死问题。
/// 所有窗口操作都在后端同步完成，确保 region-select 窗口先被隐藏。
#[tauri::command]
async fn pin_screenshot(
    app: tauri::AppHandle,
    image_path: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), String> {
    log::info!(
        "pin_screenshot 开始: path={} pos=({},{}) size={}x{}",
        image_path,
        x,
        y,
        w,
        h
    );

    // 步骤 1：先隐藏 region-select 窗口（避免与贴图窗口 z-order 冲突）
    if let Some(overlay) = app.get_webview_window("region-select") {
        match overlay.hide() {
            Ok(_) => log::info!("pin_screenshot: region-select 已隐藏"),
            Err(e) => log::error!("pin_screenshot: 隐藏 region-select 失败: {}", e),
        }
        // 取消置顶，避免隐藏后仍捕获鼠标
        let _ = overlay.set_always_on_top(false);
    } else {
        log::warn!("pin_screenshot: 未找到 region-select 窗口");
    }

    // 步骤 2：创建并显示贴图窗口
    let label = format!("pin-{}", next_pin_id(&app));
    log::info!("pin_screenshot: 贴图窗口 label={}", label);

    // 使用 App 内路由（dev 和 release 都可用，不依赖外部 HTTP 服务）
    let path = format!("/#/pin?img={}&w={}&h={}", url_encode(&image_path), w, h);
    log::info!("pin_screenshot: 路径={}", path);
    log::logger().flush();

    log::info!("pin_screenshot: 准备 build 窗口");
    log::logger().flush();

    let pin_window = WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(path.into()))
        .title("")
        .position(x, y)
        .inner_size(w, h)
        .decorations(false)
        .transparent(true)
        .shadow(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(true)
        .min_inner_size(80.0, 80.0)
        .build()
        .map_err(|e| {
            log::error!("pin_screenshot: 创建贴图窗口失败: {}", e);
            log::logger().flush();
            format!("创建贴图窗口失败: {}", e)
        })?;

    log::info!("pin_screenshot: 贴图窗口已创建，准备显示");
    log::logger().flush();
    let _ = pin_window.show();
    let _ = pin_window.set_focus();

    // 步骤 3：恢复主窗口（按截图前状态还原，不抢贴图窗口焦点）
    restore_main_after_capture(&app, "pin_screenshot");

    log::info!("pin_screenshot: 完成");
    Ok(())
}

/// 生成下一个贴图窗口 label 序号
fn next_pin_id(app: &tauri::AppHandle) -> usize {
    let mut i = 1;
    loop {
        if app.get_webview_window(&format!("pin-{}", i)).is_none() {
            return i;
        }
        i += 1;
    }
}

/// 简易 URL 编码（仅处理 Windows 路径中常见的字符）
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

/// 初始化文件日志：写入 exe 目录下 jietu-hdr.log
/// 启动时清空，运行时累加（符合项目约定）
fn init_file_logger() {
    use std::fs::OpenOptions;
    use std::io::LineWriter;

    // 获取 exe 所在目录
    let log_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("jietu-hdr.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("jietu-hdr.log"));

    // append 模式 + 超限轮转（不启动清空：第二实例（双击文件转发）也会走此
    // 初始化，truncate 会毁掉主实例的排障日志——曾因此丢失录制事故现场）
    if log_path.exists() {
        if let Ok(meta) = std::fs::metadata(&log_path) {
            if meta.len() > 2 * 1024 * 1024 {
                let rotated = log_path.with_extension("log.old");
                let _ = std::fs::remove_file(&rotated);
                let _ = std::fs::rename(&log_path, &rotated);
            }
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);

    match file {
        Ok(f) => {
            // 使用 LineWriter 包装 File，确保每行写入后立即 flush（避免日志缓冲导致看不到最新日志）
            let target: Box<dyn std::io::Write + Send> = Box::new(LineWriter::new(f));
            let _ =
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                    .format_timestamp_secs()
                    .target(env_logger::Target::Pipe(target))
                    .try_init();
            log::info!("日志文件: {}", log_path.display());
        }
        Err(e) => {
            // 降级：stderr（无 console 时不可见，但避免崩溃）
            let _ =
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                    .format_timestamp_secs()
                    .try_init();
            eprintln!("无法创建日志文件 {}: {}", log_path.display(), e);
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 初始化日志：写入 exe 目录下 jietu-hdr.log（启动时清空，运行时累加）
    init_file_logger();

    // panic 诊断钩子：release panic=abort 下 hook 仍在 abort 前执行——
    // GUI 无控制台 stderr，panic 信息默认完全丢失；写文件保留现场
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let msg = format!(
            "PANIC @ {}\n  payload: {}\n",
            info.location().map(|l| l.to_string()).unwrap_or_default(),
            info.payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_default()
        );
        // jietu-hdr.log 同目录（既有日志链路可见）+ 独立 panic 文件（防日志被截断）
        log::error!("{}", msg);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("panics.log")))
                .unwrap_or_else(|| std::path::PathBuf::from("panics.log")))
        {
            let _ = f.write_all(msg.as_bytes());
            let _ = f.write_all(b"\n");
        }
    }));

    log::info!("jietu-hdr Tauri 后端启动");

    // WMI 预热：必须在 WebView2 初始化之前注册进程级 COM 安全
    // （IMPERSONATE）。WebView2 会抢先以低模拟级别初始化，一旦它先
    // 注册，亮度控制的 WMI 调用将永远 0x80041003（无法用代理毯补救）
    capture::brightness::warmup();

    // 命令行清理入口：`jietu-hdr.exe -clean`（对照 memreduct -reduct，
    // SOURCE_CMDLINE）：执行默认 mask 清理 → 记日志 → 退出（无 --window 语义）
    if std::env::args().any(|a| a == "-clean" || a == "--clean") {
        log::info!("命令行清理启动 (-clean)，执行默认区域组合");
        let result = memory::perform_clean(0, "cmdline");
        log::info!(
            "命令行清理完成: 释放 {}（{} 个区域成功）",
            memory::format_bytes(result.freed_bytes),
            result.per_region.iter().filter(|r| r.ok).count()
        );
        std::process::exit(0);
    }

    // 自建单实例：第二实例转发 argv 后退出（mutex+事件+队列文件，见 singleinstance.rs）
    if singleinstance::try_forward_secondary() {
        std::process::exit(0);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // 全局快捷键插件
        .plugin(
            ShortcutBuilder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }
                    on_global_shortcut(app, shortcut);
                })
                .build(),
        )
        .setup(|app| {
            // 单实例监听（首实例）：处理第二实例转发的 argv 队列
            singleinstance::start_listener(app.handle().clone());

            // 文件关联自动补注册：已开启关联的用户启动时幂等重注册，
            // 保证 ASSOC_EXTS 格式清单（如新增 jxr/wdp/exr）随版本更新生效
            if Config::load().file_assoc {
                if let Err(e) = assoc::register() {
                    log::warn!("启动时刷新文件关联失败: {}", e);
                }
            }

            // 缩略图后台预热：异步渐进填充内存缓存（线程内部自等 2s，不阻塞启动）
            viewer::thumbnail::start_background_warmup();

            // 视频计划录制调度器（30s 轮询 config；幂等——仅首个实例启动）
            video::commands::start_scheduler(app.handle());

            // 真 HDR 视图预热：后台预编译 D3D 着色器 + 注册窗口类
            // （消除首次打开 100-300ms 的 D3DCompile 运行时编译延迟）
            std::thread::Builder::new()
                .name("hdr-prewarm".into())
                .spawn(|| viewer::hdr_viewer::prewarm())
                .ok();

            // 设置 Mica 云母材质（Win11）：主面板 + 看图窗口（viewer 为 transparent:true 必须有 backdrop）。
            // viewer 已改为按需懒创建（不在启动窗口清单），此处仅覆盖已存在的窗口；
            // 懒创建路径在 get_or_create_viewer_window 内自行 apply_mica
            for label in ["main", "viewer"] {
                if let Some(window) = app.get_webview_window(label) {
                    if let Ok(hwnd) = window.hwnd() {
                        apply_mica(hwnd.0 as isize);
                    }
                }
            }

            // 初始化热键状态
            let config = Config::load();
            app.manage(Mutex::new(HotkeyState {
                region: Some(config.region_hotkey),
                fullscreen: Some(config.fullscreen_hotkey),
                silent: Some(config.silent_hotkey),
                clean: config.memory_clean.hotkey,
                record_start: config.record_start_hotkey,
                record_stop: config.record_stop_hotkey,
                video_start: config.video_start_hotkey,
                video_stop: config.video_stop_hotkey,
            }));

            // 根据配置注册全局热键
            sync_hotkeys(app.handle().clone(), &config)?;

            // 恢复主窗口上次的位置与大小（记忆显示比例和大小）
            if let Some(window) = app.get_webview_window("main") {
                if let Some(ws) = config.main_window {
                    // 校验有效性：过滤最小化标志位（-32000）与异常小尺寸
                    // （历史版本可能已把无效几何写入 config.toml）
                    let valid_pos = ws.x > -30000 && ws.y > -30000;
                    // tauri.conf.json 最小 320x400（逻辑像素），按 1.0 缩放的物理像素下限
                    let valid_size = ws.width >= 320 && ws.height >= 400;
                    if valid_pos && valid_size {
                        let _ = window.set_position(tauri::PhysicalPosition::new(ws.x, ws.y));
                        let _ = window.set_size(tauri::PhysicalSize::new(ws.width, ws.height));
                        log::info!(
                            "已恢复主窗口位置大小: ({}, {}) {}x{}",
                            ws.x, ws.y, ws.width, ws.height
                        );
                    } else {
                        log::warn!(
                            "忽略无效主窗口几何: ({}, {}) {}x{}（最小化/异常尺寸），使用默认位置",
                            ws.x, ws.y, ws.width, ws.height
                        );
                    }
                }
            }
            // 落盘线程：800ms 防抖（拖动/缩放过程不频繁写盘），脏标记时合并进 config.toml
            std::thread::spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_millis(800));
                if MAIN_WIN_DIRTY.swap(false, Ordering::Relaxed) {
                    if let Some(ws) = *MAIN_WIN_STATE.lock().unwrap() {
                        let mut cfg = Config::load();
                        cfg.main_window = Some(ws);
                        if let Err(e) = cfg.save() {
                            log::warn!("主窗口状态保存失败: {:#}", e);
                        }
                    }
                }
            });

            // 启动兜底：前端就绪信号未达（脚本异常/加载失败）时 4s 强制显示主窗口，
            // 避免延迟显示模式下窗口永远不出现（启动即播放模式除外——主窗口刻意隐藏）
            {
                let app_handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(4));
                    if STARTUP_VIDEO_LAUNCH.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(w) = app_handle.get_webview_window("main") {
                        if !w.is_visible().unwrap_or(false) {
                            let _ = w.show();
                            log::warn!("[启动] 兜底：未收到前端就绪信号，强制显示主窗口");
                        }
                    }
                });
            }

            // region-select 看门狗：覆盖层可见但心跳停止 8s（渲染进程崩溃特征，
            // 表现为全屏冻结画面吞掉全部键盘/鼠标）→ 强制收起并恢复主窗口
            {
                let app_handle = app.handle().clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let Some(overlay) = app_handle.get_webview_window("region-select") else {
                        continue;
                    };
                    if !overlay.is_visible().unwrap_or(false) {
                        continue;
                    }
                    let hb = REGION_HEARTBEAT.load(Ordering::Relaxed);
                    if hb == 0 {
                        continue;
                    }
                    let stale = now_secs().saturating_sub(hb);
                    if stale > 8 {
                        log::error!(
                            "region-select 看门狗：心跳停止 {}s（渲染进程疑似崩溃），强制收起覆盖层",
                            stale
                        );
                        let _ = overlay.hide();
                        restore_main_after_capture(&app_handle, "region-select 看门狗");
                    }
                });
            }

            // 首实例启动参数（双击图片 / 右键菜单启动本程序）：
            // 前端 listener 尚未就绪，延迟 1.2s 派发避免事件丢失
            let action = parse_launch_args(&std::env::args().collect::<Vec<_>>());
            // 启动即播放（文件关联双击视频）：主窗口全程隐藏，播放器是唯一界面
            if matches!(action, LaunchAction::OpenVideo(_)) {
                STARTUP_VIDEO_LAUNCH.store(true, Ordering::Relaxed);
                log::info!("[启动] 启动即播放模式：抑制主窗口");
                // 播放器生命周期 = 应用生命周期：窗口关闭（或打开失败）→ 无界面实例退出
                let app = app.handle().clone();
                std::thread::spawn(move || {
                    // 等 PlayerWindow 创建（FFmpeg probe 大文件可达 ~10s）
                    let mut waited = 0u32;
                    while waited < 30_000 && !crate::video::player::player_alive() {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        waited += 200;
                    }
                    if !crate::video::player::player_alive() {
                        log::warn!("[启动即播放] 播放器未创建（打开失败），退出");
                        app.exit(0);
                        return;
                    }
                    // 等播放器关闭
                    while crate::video::player::player_alive() {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    log::info!("[启动即播放] 播放器已关闭，退出应用");
                    app.exit(0);
                });
            }
            if !matches!(action, LaunchAction::None) {
                let app = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                    handle_launch_action(&app, action);
                });
            }

            // 创建托盘图标 + 菜单（Tauri menu = Win32 原生菜单）
            let show_item = MenuItem::with_id(app, "tray_show", "显示主窗口", true, None::<&str>)?;
            let capture_item = MenuItem::with_id(app, "tray_capture", "立即截图", true, None::<&str>)?;
            let viewer_item = MenuItem::with_id(app, "tray_viewer", "看图", true, None::<&str>)?;
            let clean_item = MenuItem::with_id(app, "tray_clean", "立即清理", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "tray_quit", "退出", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[&show_item, &capture_item, &viewer_item, &clean_item, &separator, &quit_item],
            )?;

            let _tray = TrayIconBuilder::with_id("main-tray")
                .tooltip("jietu-hdr")
                .icon(app.default_window_icon().cloned().ok_or_else(|| {
                    anyhow::anyhow!("未找到默认窗口图标")
                })?)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    match event.id.as_ref() {
                        "tray_show" => {
                            hide_region_select_if_visible(app);
                            if let Some(window) = app.get_webview_window("main") {
                                // 最小化态必须 unminimize：仅 show() 不会还原窗口
                                let _ = window.show();
                                let _ = window.unminimize();
                                let _ = window.set_focus();
                            }
                        }
                        "tray_capture" => {
                            let app = app.clone();
                            tauri::async_runtime::spawn(async move {
                                // 托盘发起：非面板来源 —— 截图完成后不还原主窗口（不抢焦点）
                                if let Err(e) = enter_region_select_impl(&app, false) {
                                    log::error!("托盘截图失败: {}", e);
                                }
                            });
                        }
                        "tray_viewer" => {
                            // 显示看图窗口（窗口内可自行「打开」选图；不存在则懒创建）
                            match get_or_create_viewer_window(app) {
                                Ok((window, _)) => {
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                                Err(e) => log::error!("托盘打开看图窗口失败: {}", e),
                            }
                        }
                        "tray_clean" => {
                            // 立即清理：默认 mask（SOURCE_MANUAL，对照 memreduct 托盘菜单）
                            let app = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let result =
                                    tauri::async_runtime::spawn_blocking(|| memory::perform_clean(0, "manual"))
                                        .await;
                                match result {
                                    Ok(r) => memory::emit_clean_notification(&app, &r, "manual"),
                                    Err(e) => log::error!("托盘清理失败: {}", e),
                                }
                            });
                        }
                        "tray_quit" => {
                            flush_main_window_state();
                            // 退出保护：同 quit_app——录制/编码收尾未完成时同步等待
                            record::commands::flush_on_exit(app);
                            // 视频会话退出保护（同 quit_app）
                            video::commands::flush_on_exit();
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    // 双击托盘图标 → 显示主窗口（顺带收起残留覆盖层）
                    if let TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        hide_region_select_if_visible(app);
                        if let Some(window) = app.get_webview_window("main") {
                            // 最小化态必须 unminimize：仅 show() 不会还原窗口
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            // region-select 窗口已在 tauri.conf.json 中预定义
            // 设置 WDA_EXCLUDEFROMCAPTURE：窗口不参与屏幕捕获
            if let Some(overlay) = app.get_webview_window("region-select") {
                if let Ok(hwnd) = overlay.hwnd() {
                    set_window_display_affinity(hwnd.0 as isize, true);
                    // 注册句柄：Win10 GDI 兜底捕获时需临时隐藏覆盖层（BitBlt 会涂黑）
                    capture::dxgi_duplication::set_region_select_hwnd(hwnd.0 as isize);
                    log::info!("region-select 窗口已设置 EXCLUDEFROMCAPTURE");
                }
            } else {
                log::warn!("region-select 窗口未在配置中预定义");
            }

            // 内存清理调度器（1s）：刷新内存 → 事件 memory://status → 托盘百分比徽章
            // → 自动清理判定（仅提权态；对照 memreduct _app_timercallback）
            {
                let app_handle = app.handle().clone();
                std::thread::spawn(move || memory::run_scheduler(app_handle));
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            let label = window.label().to_string();
            match event {
                // 主窗口移动/缩放：记录几何状态（记忆显示比例和大小）
                tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Moved(_) if label == "main" => {
                    record_main_window_state(window);
                }
                // 主窗口：若启用 minimize_to_tray，则隐藏而非退出（隐藏前落盘窗口状态）
                tauri::WindowEvent::CloseRequested { api, .. } if label == "main" => {
                    flush_main_window_state();
                    let config = Config::load();
                    if config.minimize_to_tray {
                        let _ = window.hide();
                        api.prevent_close();
                    }
                }
                // 看图窗口 X：有标签打开 → 关当前标签回相册页（前端处理）；无标签 → 隐藏窗口
                tauri::WindowEvent::CloseRequested { api, .. } if label == "viewer" => {
                    log::info!("[trace] viewer CloseRequested（tabs_open={}）", VIEWER_TABS_OPEN.load(Ordering::Relaxed));
                    api.prevent_close();
                    if VIEWER_TABS_OPEN.load(Ordering::Relaxed) {
                        let _ = window.emit_to("viewer", "viewer://close-current-tab", ());
                    } else {
                        let _ = window.hide();
                    }
                }
                // AI 放大窗口 X：隐藏（批量任务继续后台执行）
                tauri::WindowEvent::CloseRequested { api, .. } if label == "upscale" => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // 标注窗口被关闭（任何方式）：确保主窗口恢复并能接收输入
                tauri::WindowEvent::Destroyed if label == "annotation" => {
                    let app = window.app_handle();
                    if let Some(main) = app.get_webview_window("main") {
                        // 最小化态必须 unminimize：仅 show() 不会还原窗口
                        let _ = main.show();
                        let _ = main.unminimize();
                        let _ = main.set_focus();
                    }
                }
                // 区域选择窗口被关闭：恢复主窗口（按截图前状态还原，不抢焦点）
                tauri::WindowEvent::CloseRequested { api, .. } if label == "region-select" => {
                    let app = window.app_handle();
                    let _ = window.hide();
                    api.prevent_close();
                    restore_main_after_capture(&app, "region-select 关闭");
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            frontend_main_ready,
            enter_region_select,
            capture_fullscreen,
            capture_silent,
            capture_region_at,
            capture_region_to_clipboard,
            capture_region_save_as,
            capture_region_for_annotation,
            cancel_region_select,
            open_annotation_window,
            close_annotation_window,
            commit_annotation,
            save_annotation_as,
            open_pin_window,
            pin_screenshot,
            list_windows,
            ocr_image,
            ocr_languages,
            frontend_log,
            get_temp_dir,
            get_hdr_status,
            // 面板原生色域（monitor.rs：WinRT AdvancedColorInfo 自动识别）
            capture::monitor::panel_gamut,
            // waifu2x 原生移植（P1 对拍验证）
            upscale::commands::upscale_p1_test,
            upscale::commands::upscale_window_run,
            upscale::commands::upscale_window_cancel,
            upscale::commands::upscale_models,
            upscale::commands::upscale_benchmark,
            upscale::commands::upscale_history,
            // 录屏 JXL 动图（record/）
            record::commands::record_start,
            record::commands::record_stop,
            record::commands::record_cancel,
            record::commands::record_status,
            record::commands::close_record_osd,
            // 视频录制/播放（video/，videorec 接入）
            video::commands::video_record_start,
            video::commands::video_record_stop,
            video::commands::video_record_cancel,
            video::commands::video_record_status,
            video::commands::video_record_pause,
            video::commands::video_record_resume,
            video::commands::video_cancel_shutdown,
            video::commands::video_cancel_delayed_start,
            video::commands::video_list_monitors,
            video::commands::video_list_cameras,
            video::commands::video_play_open,
            video::commands::video_play_close,
            video::commands::video_play_fx_status,
            video::commands::video_play_fx_set,
            video::commands::video_play_fx_reset,
            video::commands::close_video_osd,
            video::commands::video_component_status,
            set_hdr_state,
            get_brightness_status,
            set_brightness,
            get_config,
            save_config,
            export_config,
            import_config,
            backup_config,
            set_sdr_white_level,
            copy_image_file_to_clipboard,
            register_hotkey,
            unregister_hotkey,
            set_hotkey_capture_mode,
            enable_autostart,
            disable_autostart,
            is_autostart_enabled,
            show_main_window,
            quit_app,
            set_window_theme,
            // 内存清理（memory.rs）
            memory::get_memory_info,
            memory::clean_memory,
            memory::is_elevated,
            memory::restart_elevated,
            memory::clean_own_working_set,
            memory::get_clean_stats,
            // 看图（viewer/）
            viewer::open_image,
            viewer::list_directory_images,
            viewer::decode_image,
            viewer::pdf_render_page,
            viewer::retonemap_image,
            viewer::hdr_viewer::hdr_display_info,
            viewer::decode::itm_ai::itm_to_hdr,
            viewer::hdr_viewer::open_hdr_viewer,
            viewer::hdr_viewer::set_hdr_view_rect,
            viewer::hdr_viewer::close_hdr_viewer,
            // 动画回放（P3；JXL 动图）
            viewer::hdr_viewer::probe_animation,
        viewer::hdr_viewer::open_animation_player,
        viewer::hdr_viewer::anim_toolbar_action,
        viewer::hdr_viewer::anim_toolbar_state,
            // 路线 B 验证：VarDCT 系数导出 probe（jietu libjxl 魔改）
            viewer::decode::jxl::coeff_export_probe,
            // 调参历史 + 文件夹预解码（viewer/tonemap_history.rs）
            viewer::tonemap_history::history_add,
            viewer::tonemap_history::history_list,
            viewer::tonemap_history::history_apply,
            viewer::tonemap_history::history_export_png,
            viewer::tonemap_history::history_delete,
            viewer::tonemap_history::prewarm_folder,
            viewer::get_thumbnail,
            viewer::get_exif,
            viewer::convert_image,
            viewer::copy_image_to_clipboard,
            viewer::set_wallpaper,
            viewer::recycle_image,
            viewer::list_recycle,
            viewer::restore_recycle,
            viewer::purge_recycle,
            viewer::purge_all_recycle,
            viewer::save_image_blob,
            // 相册库（viewer/library.rs）
            viewer::library::library_state,
            viewer::library::add_library_root,
            viewer::library::remove_library_root,
            viewer::library::scan_library,
            viewer::library::set_meta,
            // 逻辑相册（viewer/albums.rs）
            viewer::albums::albums_state,
            viewer::albums::album_create_folder,
            viewer::albums::album_rename_folder,
            viewer::albums::album_delete_folder,
            viewer::albums::album_add_items,
            viewer::albums::album_remove_items,
            viewer::albums::album_move_items,
            viewer::albums::album_move_folder,
            viewer::albums::albums_import,
            viewer::albums::albums_export,
            // 批量重命名（viewer/batch.rs）
            viewer::batch::batch_rename_preview,
            viewer::batch::batch_rename,
            viewer::print::list_printers,
            viewer::print::printer_papers,
            viewer::print::scan_printable_files,
            viewer::print::print_files,
            viewer::print::print_preview,
            viewer::print::printer_properties,
            viewer::print::free_thumbs,
            viewer::print::print_templates_list,
            viewer::print::print_templates_save,
            viewer::print::print_templates_delete,
            // 加密隐私相册（viewer/vault.rs）
            viewer::vault::vault_status,
            viewer::vault::vault_setup,
            viewer::vault::vault_unlock,
            viewer::vault::vault_lock,
            viewer::vault::vault_add,
            viewer::vault::vault_list,
            viewer::vault::vault_open,
            viewer::vault::vault_restore,
            viewer::vault::vault_remove,
            viewer::vault::vault_change_password,
            // 系统集成：文件关联 + 右键菜单（assoc.rs）
            is_file_assoc,
            set_file_assoc,
            open_default_apps,
            get_assoc_detail,
            // 主面板 → 看图窗口调起（Rust emit_to 通道）
            open_viewer_image,
            open_upscale_window,
            open_viewer_library,
            hide_viewer_window,
            trace_log,
            set_main_geometry_recording,
            region_heartbeat,
            set_viewer_tabs_open,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 全局快捷键回调：根据当前运行时状态匹配触发的热键
fn on_global_shortcut(app: &tauri::AppHandle, sc: &Shortcut) {
    // 读取四个热键并判断触发的类型（含清理内存）
    let (region, fullscreen, silent, clean) = {
        let state = match app.try_state::<Mutex<HotkeyState>>() {
            Some(s) => s,
            None => return,
        };
        let st = match state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        (st.region, st.fullscreen, st.silent, st.clean)
    };

    let matches = |hk: Option<Hotkey>| {
        hk.as_ref()
            .and_then(hotkey_to_shortcut)
            .map(|x| &x == sc)
            .unwrap_or(false)
    };

    if matches(region) {
        log::info!("全局热键触发：区域截图（选择模式）");
        if let Err(e) = enter_region_select_impl(app, false) {
            log::error!("进入区域选择模式失败: {}", e);
        }
    } else if matches(fullscreen) {
        log::info!("全局热键触发：全屏截图（直接捕获+预览）");
        let app = app.clone();
        std::thread::spawn(move || {
            if let Err(e) = capture_fullscreen_impl(&app, false) {
                log::error!("全屏截图失败: {:#}", e);
            }
        });
    } else if matches(silent) {
        log::info!("全局热键触发：静默截图（直接保存）");
        let app = app.clone();
        std::thread::spawn(move || {
            if let Err(e) = capture_silent_impl(&app, false) {
                log::error!("静默截图失败: {:#}", e);
            }
        });
    } else if matches(clean) {
        // 清理内存热键（完整复刻 memreduct SOURCE_HOTKEY：默认区域组合）
        log::info!("全局热键触发：清理内存（SOURCE_HOTKEY）");
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let result =
                tauri::async_runtime::spawn_blocking(|| memory::perform_clean(0, "hotkey")).await;
            match result {
                Ok(r) => memory::emit_clean_notification(&app, &r, "hotkey"),
                Err(e) => log::error!("热键清理失败: {}", e),
            }
        });
    } else if *sc == Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyL) {
        // 隐私相册唤起（设计稿 5.2：入口可隐藏，快捷键唤起）
        log::info!("全局热键触发：隐私相册（Ctrl+Shift+L）");
        match get_or_create_viewer_window(app) {
            Ok((window, fresh)) => {
                let _ = window.show();
                let _ = window.set_focus();
                if fresh {
                    // 新建窗口前端 listener 尚未挂载，等待就绪再派发（防事件丢失）
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        let _ = window.emit_to("viewer", "viewer://vault", ());
                    });
                } else {
                    let _ = window.emit_to("viewer", "viewer://vault", ());
                }
            }
            Err(e) => log::error!("隐私相册唤起失败（viewer 窗口创建失败）: {}", e),
        }
    } else if is_record_start_hotkey(app, sc) {
        // 开始录制热键（游戏模式直启：deferred+osd；已有会话自动忽略）
        log::info!("全局热键触发：开始录制（游戏模式）");
        record::commands::start_hotkey_triggered(app);
    } else if is_record_stop_hotkey(app, sc) {
        // 录制停止热键（沉浸式游戏模式临时注册；无会话时容错忽略）
        log::info!("全局热键触发：停止录制");
        record::commands::stop_hotkey_triggered(app);
    } else if is_video_start_hotkey(app, sc) {
        // 开始视频录制热键（游戏模式直启 MKV + OSD；已有会话自动忽略）
        log::info!("全局热键触发：开始录制视频（游戏模式）");
        video::commands::start_hotkey_triggered(app);
    } else if is_video_stop_hotkey(app, sc) {
        // 视频录制停止热键（沉浸式临时注册；无会话时容错忽略）
        log::info!("全局热键触发：停止录制视频");
        video::commands::stop_hotkey_triggered(app);
    }
}

/// 触发的快捷键是否为「开始录制」热键（按运行时配置匹配）
fn is_record_start_hotkey(app: &tauri::AppHandle, sc: &Shortcut) -> bool {
    is_state_hotkey(app, sc, |st| st.record_start)
}

/// 触发的快捷键是否为「停止录制」热键（按运行时配置匹配）
fn is_record_stop_hotkey(app: &tauri::AppHandle, sc: &Shortcut) -> bool {
    is_state_hotkey(app, sc, |st| st.record_stop)
}

/// 触发的快捷键是否为「开始录制视频」热键（按运行时配置匹配）
fn is_video_start_hotkey(app: &tauri::AppHandle, sc: &Shortcut) -> bool {
    is_state_hotkey(app, sc, |st| st.video_start)
}

/// 触发的快捷键是否为「停止录制视频」热键（按运行时配置匹配）
fn is_video_stop_hotkey(app: &tauri::AppHandle, sc: &Shortcut) -> bool {
    is_state_hotkey(app, sc, |st| st.video_stop)
}

fn is_state_hotkey(
    app: &tauri::AppHandle,
    sc: &Shortcut,
    pick: impl Fn(&HotkeyState) -> Option<Hotkey>,
) -> bool {
    // MutexGuard 借用 try_state 的临时 State，不可跨 and_then 链返回，拆开持有
    let Some(state) = app.try_state::<Mutex<HotkeyState>>() else {
        return false;
    };
    let Ok(st) = state.lock() else {
        return false;
    };
    pick(&st)
        .and_then(|hk| hotkey_to_shortcut(&hk))
        .map(|matched| matched == *sc)
        .unwrap_or(false)
}

/// 设置窗口 Mica 云母材质（Windows 11）
///
/// 用 raw HWND (isize) 和 raw 常量值调用，避免类型/版本冲突。
/// DWMWA_SYSTEMBACKDROP_TYPE = 38, DWMSBT_MAIN = 2 (Mica)
fn apply_mica(hwnd_raw: isize) {
    // 默认浅色 Mica；深色由 set_window_theme 命令切 Mica-Alt
    apply_window_backdrop(hwnd_raw, false);
}

/// 设置窗口材质：浅色 Mica (DWMSBT_MAINWINDOW=2) / 深色 Mica-Alt (DWMSBT_TABBEDWINDOW=4)
/// 同时显式声明 DWM 圆角偏好（DWMWCP_ROUND，最大化时系统自动变直角）
fn apply_window_backdrop(hwnd_raw: isize, dark: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_WINDOW_CORNER_PREFERENCE,
    };
    let hwnd = HWND(hwnd_raw as *mut _);
    unsafe {
        let backdrop: i32 = if dark { 4 } else { 2 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as *const _,
            std::mem::size_of::<i32>() as u32,
        );
        // Win11 原生圆角（DWMWCP_ROUND=2；系统上限 ~8px，最大化自动消失）
        let corner: i32 = 2;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const _,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

/// 主题切换：前端根据当前模式调用，深色切 Mica-Alt
///
/// 同时作用于主面板与看图窗口（viewer 为 transparent:true，
/// 必须有 Mica backdrop 才能正常显示背景）
#[tauri::command]
fn set_window_theme(app: tauri::AppHandle, dark: bool) -> Result<(), String> {
    for label in ["main", "viewer"] {
        if let Some(w) = app.get_webview_window(label) {
            if let Ok(hwnd) = w.hwnd() {
                apply_window_backdrop(hwnd.0 as isize, dark);
            }
        }
    }
    log::info!(
        "窗口材质已切换: {}",
        if dark {
            "Mica-Alt(深色)"
        } else {
            "Mica(浅色)"
        }
    );
    Ok(())
}

/// 设置窗口显示亲和性（WDA）
///
/// exclude=true → WDA_EXCLUDEFROMCAPTURE（不参与屏幕捕获，DXGI 捕获中不可见）
/// exclude=false → WDA_NONE（恢复正常捕获）
fn set_window_display_affinity(hwnd_raw: isize, exclude: bool) {
    // SetWindowDisplayAffinity 在 user32.dll 中，通过 raw extern 调用
    // 避免不同版本 windows crate 的模块路径差异
    extern "system" {
        fn SetWindowDisplayAffinity(hwnd: *mut std::ffi::c_void, dwaffinity: u32) -> i32;
    }
    const WDA_NONE: u32 = 0x00000000;
    // WDA_EXCLUDEFROMCAPTURE = 0x00000011 (17)
    const WDA_EXCLUDEFROMCAPTURE: u32 = 0x00000011;
    let affinity = if exclude {
        WDA_EXCLUDEFROMCAPTURE
    } else {
        WDA_NONE
    };
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd_raw as *mut std::ffi::c_void, affinity);
    }
}

/// 截图期间临时将主窗口排除出捕获（置顶 + 配置不含工具时）
///
/// 置顶（钉住）状态下主面板保持显示；capture_include_tool=false 时
/// 用 WDA_EXCLUDEFROMCAPTURE 让面板不入镜。返回需恢复的窗口句柄，
/// 捕获完成后调用 [`restore_main_exclusion`]。
fn exclude_main_if_needed(app: &tauri::AppHandle, config: &Config) -> Option<isize> {
    let main = app.get_webview_window("main")?;
    let pinned = main.is_always_on_top().unwrap_or(false);
    if !pinned || config.capture_include_tool {
        return None;
    }
    let hwnd = main.hwnd().ok().map(|h| h.0 as isize)?;
    log::info!("置顶截图：主面板临时排除出捕获");
    set_window_display_affinity(hwnd, true);
    // 等合成器应用亲和性（实测立即生效，留短缓冲保险）
    std::thread::sleep(std::time::Duration::from_millis(50));
    Some(hwnd)
}

/// 恢复主窗口捕获亲和性（配合 [`exclude_main_if_needed`]）
fn restore_main_exclusion(hwnd: Option<isize>) {
    if let Some(h) = hwnd {
        set_window_display_affinity(h, false);
        log::info!("置顶截图：主面板恢复捕获");
    }
}
