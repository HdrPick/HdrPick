//! 录制 Tauri 命令
//!
//! 前端流程：`record_start`（选区后调用，立即返回）→ 每秒 `record_status`
//! 或监听 `record://stats`（倒计时）→ `record_stop`（阻塞至 finalize 完成，
//! 桌面场景通常数秒）/ `record_cancel`（放弃并删除半成品）。
//!
//! 游戏模式（沉浸式）：`record_start({ osd: true })` → 主面板最小化 +
//! 创建 OSD 叠加窗（透明置顶小窗、点击穿透、不入截图、不抢焦点）+
//! 临时注册停止热键（默认 Alt+F10，录制结束自动注销）→ 全程无面板交互，
//! 停止/完成由热键与 OSD 提示完成。
//!
//! 游戏模式静默延迟启动（`start_delay_seconds`，默认 5s，0 = 立即）：
//! 沉浸式布置完成后先倒计时再 spawn 抓帧——给用户切回游戏的时间，
//! 期间游戏焦点不被抢、OSD 静默显示倒计时；30s 录制上限从抓帧开始计。
//!
//! 事件（后端 → 前端）：
//! - `record://stats`     每秒统计（elapsed/captured/deduped/encoded/ring 水位）
//! - `record://countdown` 延迟启动倒计时（负载 `{seconds: n}`；n 递减到 1，
//!                        结束补发 `{seconds: 0}` = 开始录制）
//! - `record://autostop`  到达 30s 上限自动停止
//! - `record://done`      finalize 完成（负载 = RecordResult）
//! - `record://error`     致命错误

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, WebviewWindowBuilder};

use crate::config::Config;
use crate::encode::QualityLevel;

use super::recorder::{self, CaptureBackend, RecordRegion, Session};

/// SESSION 槽位：
/// - `Pending`：游戏模式延迟启动中（倒计时占位）——会话尚未 spawn，无抓帧
///   线程、无数据；占位使 `session_lock().is_some()` 的防重入检查在倒计时
///   期间同样生效（否则倒计时内再按 Alt+F9 会重复启动）。期间用户按停止
///   热键 → take_session 取走占位 → finish_slot 撤销沉浸式布置（等效取消），
///   倒计时线程醒来发现槽位已空即放弃启动。
/// - `Active`：已 spawn 的录制会话。
enum SessionSlot {
    Pending {
        app: AppHandle,
    },
    Active(Session),
}

/// 全局会话（同一时刻仅一个录制/一次倒计时）
static SESSION: Mutex<Option<SessionSlot>> = Mutex::new(None);

/// 录制中临时注册的停止热键（Option<Hotkey>；结束注销）
static STOP_HOTKEY: Mutex<Option<crate::config::Hotkey>> = Mutex::new(None);

/// 退出保护：会话收尾进行中标志。stop/取消已把会话从 SESSION 取走但收尾
/// （延迟编码可达 ~1-2 分钟）仍在后台进行时，退出路径 flush_on_exit 据此
/// 等待完成——否则 app.exit 杀掉编码线程，产物截断在最后写入处
static FINISHING: AtomicBool = AtomicBool::new(false);

fn session_lock() -> std::sync::MutexGuard<'static, Option<SessionSlot>> {
    // static Mutex 在 Rust ≥1.63 为 const 构造；毒化时恢复（录制线程 panic
    // 不应永久锁死后续录制入口）
    match SESSION.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 动图录制是否活跃（含倒计时占位/收尾中）——video 模块的全局录制互斥检查用
pub fn is_recording_active() -> bool {
    session_lock().is_some() || FINISHING.load(Ordering::Acquire)
}

/// 开始录制（区域坐标为虚拟屏幕物理像素；None = 全屏）
///
/// - `deferred`：纯录制模式（游戏场景）——录制期间不启动编码线程（CPU 全归
///   前台应用、环形缓冲全程 zstd），停止后 NORMAL 优先级全核编码。None = 配置默认。
/// - `osd`：沉浸式游戏模式——主面板最小化 + 创建 OSD 叠加窗（置顶/点击穿透/
///   不入截图/不抢焦点）+ 临时注册停止热键 Alt+F10。停止热键 = 停止并保存。
///
/// 返回 `{ path, fps, width, height, maxSeconds, deferred, stopHotkey }`
///
/// async + spawn_blocking：同步 command 在主线程执行，而 start_recording_inner
/// 内部 run_on_main_thread 创建 OSD 窗口——主线程等自己 = 死锁（点开始无响应根因）
#[tauri::command]
pub async fn record_start(
    app: AppHandle,
    region: Option<RecordRegion>,
    fps: Option<u32>,
    quality: Option<QualityLevel>,
    deferred: Option<bool>,
    osd: Option<bool>,
) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        start_recording_inner(&app, region, fps, deferred, osd, false)
    })
    .await
    .map_err(|e| format!("录制任务失败: {}", e))?
}

/// 录制启动核心（record_start 命令与 Alt+F9 热键共用）
///
/// osd 路径顺序：setup_immersive（OSD + 停止热键 + 面板最小化）→ 可选倒计时
/// （`start_delay_seconds`，期间 SESSION 以 `Pending` 占位防重入、不抓帧）→
/// spawn_session。倒计时期间按停止热键 = 取走占位并撤销布置（无数据可保存），
/// 倒计时线程醒来发现槽位已空即放弃启动。
fn start_recording_inner(
    app: &AppHandle,
    region: Option<RecordRegion>,
    fps: Option<u32>,
    deferred: Option<bool>,
    osd: Option<bool>,
    from_hotkey: bool,
) -> Result<serde_json::Value, String> {
    let mut guard = session_lock();
    if guard.is_some() {
        return Err("已有录制进行中".into());
    }
    // 全局录制互斥：视频（video/）在录时拒绝动图（反向检查在 video 侧）
    if crate::video::commands::is_video_recording() {
        return Err("视频录制进行中，请先停止".into());
    }
    let cfg = Config::load();
    let quality = cfg.recording.quality;
    let deferred = deferred.unwrap_or(cfg.recording.deferred_encode);
    let osd = osd.unwrap_or(false);
    // 采集后端解析（config × osd 矩阵，此处一次解析、spawn_session 只按 enum 构造）：
    // - "wgc" / "dda"：强制指定（失败即报错，不兜底）
    // - 其他值（含 "auto"）→ 自动：游戏模式（osd）WGC 优先 + DDA 兜底
    //   （Auto 语义在 spawn_session：for_monitor 失败 log warn 回退 DDA）；
    //   桌面录制保持 DDA（现状）。
    // 热键路径 start_hotkey_triggered 硬编码 osd=true → auto 自动走 WGC 优先。
    let capture_backend = match cfg.recording.capture_backend.as_str() {
        "wgc" => CaptureBackend::Wgc,
        "dda" => CaptureBackend::Dda,
        _ => {
            if osd {
                CaptureBackend::Auto
            } else {
                CaptureBackend::Dda
            }
        }
    };
    // 录制时长上限（config 解析层已钳制 1..=30；recorder 侧再钳一次兜底）
    let max_seconds = cfg.recording.max_seconds;
    // 游戏模式静默延迟启动（config 解析层已钳制 0..=10；0 = 立即启动）。
    // 非 osd 路径（桌面录制）不延迟
    let start_delay = if osd {
        cfg.recording.start_delay_seconds
    } else {
        0
    };

    // 输出路径：录制保存目录（未配置则沿用截图目录）
    let dir = cfg
        .recording
        .save_dir
        .clone()
        .unwrap_or_else(|| cfg.resolved_save_dir());
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建保存目录失败: {}", e))?;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = dir.join(format!("record_{}.jxl", ts));

    // 沉浸式布置（OSD 窗口 + 停止热键 + 主面板最小化）在会话 spawn 之前：
    // - 倒计时需要 OSD 显示 + 停止热键在倒计时期间即已注册可用
    // - 布置失败时会话尚未启动，无需回滚会话（teardown 还原面板即可）
    let stop_hotkey_desc = if osd {
        match setup_immersive(app, from_hotkey) {
            Ok(d) => d,
            Err(e) => {
                log::error!("[record] 沉浸式布置失败，还原面板: {}", e);
                teardown_immersive(app);
                return Err(e);
            }
        }
    } else {
        None
    };

    // 静默延迟启动：倒计时期间 SESSION 以 Pending 占位（Alt+F9 的 is_some
    // 检查依赖占位才不会重复启动）。倒计时每秒 sleep，必须先释放锁——
    // 否则 record_status 等命令被阻塞数秒
    if start_delay > 0 {
        log::info!(
            "[record] 延迟启动：游戏模式 {}s 后开始录制（倒计时开始）",
            start_delay
        );
        *guard = Some(SessionSlot::Pending { app: app.clone() });
        drop(guard);
        for n in (1..=start_delay).rev() {
            let _ = app.emit("record://countdown", serde_json::json!({ "seconds": n }));
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        // 倒计时结束（seconds=0 = 前端切回正常计时态的信号之一）
        let _ = app.emit("record://countdown", serde_json::json!({ "seconds": 0 }));
        log::info!("[record] 倒计时结束，启动抓帧会话");

        // 占位可能已被取走（倒计时期间按了停止热键/面板停止 = 撤销启动）
        guard = session_lock();
        if guard.is_none() {
            log::info!("[record] 倒计时期间被停止，放弃启动会话");
            return Ok(serde_json::json!({ "cancelled": true }));
        }
    }

    let session = recorder::spawn_session(recorder::SpawnParams {
        region,
        quality,
        fps: fps.or({
            let f = cfg.recording.fps;
            if f > 0 {
                Some(f)
            } else {
                None
            }
        }),
        max_seconds,
        output_path: path.clone(),
        ring_bytes: cfg.recording.ring_bytes_mb * 1024 * 1024,
        zstd_watermark: cfg.recording.zstd_watermark,
        deferred_encode: deferred,
        capture_backend,
        app: app.clone(),
    })
    .map_err(|e| format!("录制启动失败: {:#}", e))?;

    let info = serde_json::json!({
        "path": path.to_string_lossy(),
        "fps": session.fps,
        "width": session.width,
        "height": session.height,
        "maxSeconds": max_seconds,
        "deferred": deferred,
        "stopHotkey": stop_hotkey_desc,
    });
    *guard = Some(SessionSlot::Active(session));
    Ok(info)
}

/// 沉浸式游戏模式布置：OSD 叠加窗 + 停止热键 + 主面板最小化
///
/// 返回停止热键描述（OSD 提示文案用）
///
/// `from_hotkey`：热键直启（游戏模式）传 true——跳过主面板最小化。
/// 游戏全屏时主面板要么在托盘（隐藏态 minimize 会造成任务栏死条目）要么
/// 在游戏后方（不可见，最小化无意义），而 minimize 动画本身会扰动 DWM
/// 合成 → 全屏游戏闪黑屏 + 焦点被夺走 → 用户点回游戏又闪一次（双闪根因）。
fn setup_immersive(app: &AppHandle, from_hotkey: bool) -> Result<Option<String>, String> {
    // 1. 主面板最小化（不隐藏——用户仍可从任务栏找回；不抢焦点）——仅面板
    //    路径（面板可见即将被录进画面）；热键路径游戏全屏覆盖，跳过防焦点扰动
    if !from_hotkey {
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.minimize();
        }
    }

    // 2. OSD 叠加窗（存在则复用——快速连续录制场景）
    //
    // 崩溃教训：Tauri v2 的窗口创建（WebviewWindowBuilder::build）必须在主线程，
    // 从命令线程直接 build 会 panic（release 下 panic=abort → 进程秒退）。
    // 修复：用 run_on_main_thread 转发窗口创建 + 样式设置。
    if app.get_webview_window("record-osd").is_none() {
        let app2 = app.clone();
        let result: std::sync::mpsc::Receiver<Result<(), String>> = {
            let (tx, rx) = std::sync::mpsc::channel();
            app.run_on_main_thread(move || {
                let r = (|| -> Result<(), String> {
                    let window = WebviewWindowBuilder::new(
                        &app2,
                        "record-osd",
                        tauri::WebviewUrl::App("#/record-osd".into()),
                    )
                    .title("")
                    .decorations(false)
                    .transparent(true)
                    .shadow(false)
                    .always_on_top(true)
                    .skip_taskbar(true)
                    .resizable(false)
                    // 先不可见创建：build→ShowWindow 之间存在激活竞窗（Tauri
                    // focused(false) 只影响初始 ShowWindow，样式设置前的窗口
                    // 系统仍可能激活）——先建窗→设 NOACTIVATE→再 SW_SHOWNOACTIVATE
                    .visible(false)
                    // 不抢焦点：Tauri v2 窗口创建默认聚焦——全屏游戏会被顶回桌面
                    // （黑屏闪动）的根因。配合下方 WS_EX_NOACTIVATE 双保险
                    .focused(false)
                    .inner_size(240.0, 44.0)
                    .position(9999.0, 24.0) // 先放屏幕外，前端挂载后由 OSD 自行定位到右上
                    .build()
                    .map_err(|e| format!("创建 OSD 窗口失败: {}", e))?;

                    // 点击穿透（WS_EX_TRANSPARENT + WS_EX_LAYERED）：游戏中不拦截任何输入
                    // 注：tauri hwnd() 返回的 HWND 来自 tauri 内嵌 windows crate（版本与
                    // 本项目 0.58 不同），须经 .0 as isize 转本项目原生 HWND
                    if let Ok(hwnd) = window.hwnd() {
                        let hwnd_raw = hwnd.0 as isize;
                        let hwnd = windows::Win32::Foundation::HWND(hwnd_raw as *mut core::ffi::c_void);
                        unsafe {
                            let style = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                                hwnd,
                                windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
                            );
                            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                                hwnd,
                                windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
                                style
                                    | windows::Win32::UI::WindowsAndMessaging::WS_EX_TRANSPARENT.0 as isize
                                    | windows::Win32::UI::WindowsAndMessaging::WS_EX_LAYERED.0 as isize
                                    | windows::Win32::UI::WindowsAndMessaging::WS_EX_NOACTIVATE.0 as isize,
                            );
                        }
                        // 不入截图/录屏（与 region-select 同款排除）
                        crate::set_window_display_affinity(hwnd_raw, true);
                        // 样式就绪后再显示（SW_SHOWNOACTIVATE = 不激活不抢焦点）
                        unsafe {
                            let _ =
                                windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                                    hwnd,
                                    windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
                                );
                        }
                        // OSD 保持置顶看护线程：全屏游戏重新获焦时 Windows 会把
                        // 全屏窗提到 topmost 之上（OSD 沉到游戏后 = 时间信息消失
                        // 根因）——周期性重申 HWND_TOPMOST 压回最顶层
                        start_osd_keep_top(hwnd_raw as isize);
                    }

                // 位置兜底：直接定位到主屏右上角（此前依赖前端挂载后自行定位，
                // 一旦失败 OSD 停在屏幕外 (9999,24) = 用户视角"点击开始无效"）
                match window.primary_monitor() {
                    Ok(Some(mon)) => {
                        let size = window
                            .outer_size()
                            .unwrap_or(tauri::PhysicalSize::new(240, 44));
                        let margin = (16.0 * mon.scale_factor()).round() as i32;
                        let x = mon.position().x + mon.size().width as i32
                            - size.width as i32
                            - margin;
                        let y = mon.position().y + margin;
                        let r = window.set_position(tauri::PhysicalPosition::new(x, y));
                        log::info!(
                            "[record] OSD 定位: mon pos=({},{}) size=({},{}) scale={} outer=({},{}) → ({},{}) 结果={}",
                            mon.position().x, mon.position().y,
                            mon.size().width, mon.size().height,
                            mon.scale_factor(),
                            size.width, size.height, x, y,
                            r.is_ok()
                        );
                    }
                    Ok(None) => log::warn!("[record] OSD 定位: primary_monitor=None"),
                    Err(e) => log::warn!("[record] OSD 定位: primary_monitor 错误: {}", e),
                }
                let after = window.outer_position();
                log::info!(
                    "[record] OSD 定位后 outer_position={:?}",
                    after.map(|p| (p.x, p.y))
                );
                Ok(())
                })();
                let _ = tx.send(r);
            })
            .map_err(|e| format!("主线程队列不可用，无法创建 OSD 窗口: {}", e))?;
            rx
        };
        result
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| format!("OSD 窗口创建超时: {}", e))??;
    }

    // 3. 停止热键（可配置，默认 Alt+F10；None = 不注册，仅面板/OSD 停止；
    //    已占用则降级——OSD 提示用面板停止）
    let hk = crate::config::Config::load().record_stop_hotkey;
    let final_desc = hk.as_ref().and_then(|k| crate::hotkey_display_name(k));
    let registered = match hk {
        Some(k) => crate::try_register_stop_hotkey(app, k),
        None => false,
    };
    let final_desc = if registered { final_desc } else { None };
    *STOP_HOTKEY.lock().unwrap() = if registered { hk } else { None };
    if let Some(k) = hk {
        if !registered {
            log::warn!(
                "[record] 停止热键注册失败（mods=0x{:X} vk=0x{:02X}，被占用），仅可用面板/OSD 停止",
                k.modifiers,
                k.vk
            );
        }
    }
    Ok(final_desc)
}

/// 录制中补注册停止热键（reregister_all_hotkeys 的 unregister_all 会把它清掉：
/// 配置保存/热键录入结束都会走到那里；此函数仅在会话活跃时补回）
pub fn reregister_stop_hotkey_if_active(app: &AppHandle) {
    if session_lock().is_none() {
        return;
    }
    if let Some(hk) = STOP_HOTKEY.lock().unwrap().clone() {
        crate::try_register_stop_hotkey(app, hk);
    }
}

/// 沉浸式收尾：注销停止热键 + 销毁 OSD 窗 + 还原主面板
fn teardown_immersive(app: &AppHandle) {
    // 1. 注销停止热键
    let hk = STOP_HOTKEY.lock().unwrap().take();
    if let Some(hk) = hk {
        crate::try_unregister_stop_hotkey(app, hk);
    }
    // 2. 停止 OSD 置顶看护线程
    stop_osd_keep_top();
    // 3. 销毁 OSD 窗（延迟 3s 由前端 invoke close_record_osd 触发；
    //    此处兜底：done/error 事件后前端自会关闭，异常路径强制销毁）
    // 4. 主面板还原——仅面板路径（我们最小化过的）才还原；热键路径游戏全屏
    //    中，强制 show/unminimize 会把面板顶出（再次扰动游戏焦点 + 闪屏）。
    //    is_minimized 判定：我们最小化的面板恒为 true；热键路径未动过面板恒为 false
    if let Some(main) = app.get_webview_window("main") {
        if main.is_minimized().unwrap_or(false) {
            let _ = main.unminimize();
            let _ = main.show();
        }
    }
}

/// OSD 置顶看护开关（true = 看护线程运行中）
static OSD_KEEP_TOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动 OSD 置顶看护线程：每 500ms 重申 HWND_TOPMOST。
///
/// 全屏（独占/优化全屏）游戏获焦时，Windows 把它提到所有 topmost 窗之上
/// → OSD（录制时间）沉到游戏后面消失。周期性 SetWindowPos(HWND_TOPMOST)
/// 压回最顶层（无边框全屏游戏有效；真独占全屏系统层面无窗可叠加，属 OS 限制）。
/// SWP_NOACTIVATE 保证看护本身绝不抢焦点。
fn start_osd_keep_top(hwnd_raw: isize) {
    OSD_KEEP_TOP.store(true, std::sync::atomic::Ordering::Release);
    std::thread::Builder::new()
        .name("osd-keep-top".into())
        .spawn(move || {
            use std::sync::atomic::Ordering;
            while OSD_KEEP_TOP.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if !OSD_KEEP_TOP.load(Ordering::Acquire) {
                    break;
                }
                let hwnd = windows::Win32::Foundation::HWND(hwnd_raw as *mut core::ffi::c_void);
                unsafe {
                    // 窗口已销毁（IsWindow 校验）则退出看护
                    if !windows::Win32::UI::WindowsAndMessaging::IsWindow(hwnd).as_bool() {
                        break;
                    }
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                        hwnd,
                        windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST,
                        0,
                        0,
                        0,
                        0,
                        windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                            | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                            | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE
                            | windows::Win32::UI::WindowsAndMessaging::SWP_NOOWNERZORDER,
                    );
                }
            }
        })
        .ok();
}

/// 停止 OSD 置顶看护线程（线程在 ≤500ms 内自然退出）
fn stop_osd_keep_top() {
    OSD_KEEP_TOP.store(false, std::sync::atomic::Ordering::Release);
}

/// OSD 自毁（前端 done/error 展示 2.6s 后调用）
#[tauri::command]
pub fn close_record_osd(app: AppHandle) -> Result<(), String> {
    stop_osd_keep_top();
    if let Some(w) = app.get_webview_window("record-osd") {
        let _ = w.close();
    }
    Ok(())
}

/// 停止录制并等待 finalize（阻塞至编码完成；桌面场景通常数秒）
///
/// 必须 async + spawn_blocking：Tauri v2 同步 command 在主线程执行，
/// finalize 可达数十秒 → 主线程冻结；且 start 路径里 run_on_main_thread
/// 在主线程上等待自身 = 死锁（"点击开始无效/无响应"的根因）
#[tauri::command]
pub async fn record_stop() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let slot = take_session()?;
        finish_slot(slot, false)
    })
    .await
    .map_err(|e| format!("录制任务失败: {}", e))?
}

/// 停止热键入口（on_global_shortcut 调用；后台线程执行——finalize 可达数十秒，
/// 不能阻塞热键分发线程）。仅在有会话时生效（无会话 = 热键不该存在，容错忽略）
pub fn stop_hotkey_triggered(app: &AppHandle) {
    let slot = match take_session() {
        Ok(s) => s,
        Err(_) => return,
    };
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("record-stop-hotkey".into())
        .spawn(move || {
            // OSD 切"编码中"状态（立即反馈，编码转圈）
            let _ = app.emit("record://autostop", ());
            match finish_slot(slot, false) {
                Ok(_) => {}
                Err(e) => {
                    let _ = app.emit("record://error", e);
                }
            }
        });
    if spawned.is_err() {
        // 线程创建失败收尾无法进行：清退出保护标志，避免退出路径空等超时
        FINISHING.store(false, Ordering::Release);
    }
}

/// 开始录制热键（Alt+F9）：游戏模式直启全屏录制 + 沉浸式布置
///
/// 语义：专为游戏场景设计——不切出游戏即可开始（deferred 零编码 + OSD 提示
/// + Alt+F10 停止）。已有会话时忽略（防连按重复启动）。永久注册（应用启动
/// 即有效，与停止热键的"临时注册"不同）。
pub fn start_hotkey_triggered(app: &AppHandle) {
    // 已有会话/倒计时占位 → 忽略（防连按；倒计时期间同样不重复启动）
    if session_lock().is_some() {
        return;
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("record-start-hotkey".into())
        .spawn(move || {
            // 游戏模式直启：deferred + osd 全开；from_hotkey = 跳过面板最小化
            //（游戏全屏覆盖，minimize 动画扰动 DWM = 黑屏闪动 + 焦点被夺根因）
            match start_recording_inner(&app, None, None, Some(true), Some(true), true) {
                Ok(info) => {
                    // cancelled = 倒计时期间被用户停止：未真正开始，面板不切 recording 态
                    if info.get("cancelled").is_none() {
                        let _ = app.emit("record://hotkey-started", ());
                    }
                }
                Err(e) => {
                    log::error!("[record] 热键启动录制失败: {}", e);
                    let _ = app.emit("record://error", e);
                }
            }
        })
        .ok();
}

/// 取消录制（放弃半成品并删除文件）
#[tauri::command]
pub async fn record_cancel() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let slot = take_session()?;
        finish_slot(slot, true).map(|_| ())
    })
    .await
    .map_err(|e| format!("录制任务失败: {}", e))?
}

/// 录制状态（无会话时 recording=false；倒计时占位 = 已在录制流程中）
#[tauri::command]
pub fn record_status() -> serde_json::Value {
    let guard = session_lock();
    match guard.as_ref() {
        Some(SessionSlot::Active(s)) => {
            let payload = s
                .shared
                .snapshot(&s.ring, &s.output_path.to_string_lossy(), true);
            serde_json::to_value(payload)
                .unwrap_or_else(|_| serde_json::json!({ "recording": true }))
        }
        Some(SessionSlot::Pending { .. }) => serde_json::json!({ "recording": true }),
        None => serde_json::json!({ "recording": false }),
    }
}

fn take_session() -> Result<SessionSlot, String> {
    let mut guard = session_lock();
    match guard.take() {
        Some(s) => {
            // 退出保护：仍在 SESSION 锁内置位——flush_on_exit 之后观察到
            // "会话为空"时，经锁的 happens-before 必能看到此标志，不会漏等
            // 后台收尾（关闭"取走→置位前"的竞态窗口）
            FINISHING.store(true, Ordering::Release);
            Ok(s)
        }
        None => Err("没有进行中的录制".to_string()),
    }
}

/// 抓帧线程致命错误后的自动收尾（recorder.rs 抓帧循环 Err 分支调用）。
/// 修复"僵尸会话"：抓帧线程死亡只置 stop 标志时——统计停推（OSD 冻在 00:00）、
/// 30s 自动停失效（检查在死循环里）、SESSION 残留会话直到用户手动停止。
/// 此处：先发 record://error（前端既有双保险监听复位 UI），再后台取走会话
/// 做与停止等价的收尾（写出已捕获的半成品，数据安全优先），会话不留僵尸。
pub(crate) fn autofinish_capture_error(app: &tauri::AppHandle, msg: String) {
    let _ = app.emit("record://error", format!("录制中止: {}", msg));
    let app = app.clone();
    std::thread::Builder::new()
        .name("record-autofinish".into())
        .spawn(move || {
            // take 失败 = 用户已抢先手动停止（record_stop/cancel），交由其收尾
            if let Ok(slot) = take_session() {
                match finish_slot(slot, false) {
                    Ok(_) => log::info!("[record] 抓帧错误后自动收尾完成（半成品已写出）"),
                    Err(e) => log::error!("[record] 抓帧错误后自动收尾失败: {}", e),
                }
            }
        })
        .ok();
}

/// 槽位统一收尾入口（stop/cancel/热键/自动收尾/退出保护共用）：
/// - `Active` → finish_session（join 三线程 + 纯录制补启编码 + 取结果）
/// - `Pending`（倒计时占位，spawn_session 未跑）→ 撤销沉浸式布置并销毁 OSD
///   （无数据可保存，等效取消启动）。占位无任何线程句柄，join 天然不触发。
fn finish_slot(slot: SessionSlot, cancel: bool) -> Result<serde_json::Value, String> {
    match slot {
        SessionSlot::Active(session) => finish_session(session, cancel),
        SessionSlot::Pending { app } => {
            // RAII 清 FINISHING（take_session 已置位；不清理会导致
            // flush_on_exit 误判"收尾进行中"空等 180s）
            let _finishing = FinishingGuard;
            log::info!("[record] 倒计时期间停止：会话未启动，已撤销沉浸式布置");
            teardown_immersive(&app);
            // OSD 直接销毁（无结果可展示）
            if let Some(w) = app.get_webview_window("record-osd") {
                let _ = w.close();
            }
            Ok(serde_json::json!({ "cancelled": true }))
        }
    }
}

/// 停止/取消统一收尾：置标志 → join 抓帧/回拷 →（纯录制模式补启编码）→ join 编码 → 取结果
fn finish_session(mut session: Session, cancel: bool) -> Result<serde_json::Value, String> {
    // 退出保护：收尾期间置位（RAII 清除，所有退出路径/panic 均不残留）
    let _finishing = FinishingGuard;
    FINISHING.store(true, Ordering::Release);
    if cancel {
        session.shared.cancel.store(true, Ordering::Release);
    }
    session.shared.stop.store(true, Ordering::Release);
    // 三线程经队列关闭链自行退出（无死锁：票据→环形依次关闭，见 recorder.rs 头注）

    let join = |h: &mut Option<std::thread::JoinHandle<()>>| {
        if let Some(handle) = h.take() {
            let _ = handle.join();
        }
    };
    // 1. 抓帧 + 回拷先行退出（ring close 链完成，数据完整）
    join(&mut session.capture_handle);
    log::info!("[record] finish: 抓帧线程已退出");
    join(&mut session.inflight_handle);
    log::info!("[record] finish: 回拷线程已退出");

    // 沉浸式收尾：注销停止热键 + 还原主面板（OSD 窗由前端展示结果后自毁）
    teardown_immersive(&session.app);

    if cancel {
        // 取消：OSD 直接销毁（无结果可展示）
        if let Some(w) = session.app.get_webview_window("record-osd") {
            let _ = w.close();
        }
        let _ = std::fs::remove_file(&session.output_path);
        log::info!(
            "[record] 已取消，半成品已删除: {}",
            session.output_path.display()
        );
        return Ok(serde_json::json!({ "cancelled": true }));
    }

    // 2. 纯录制模式：此时才启动编码（NORMAL 优先级全核——录制已结束，
    //    不再与前台应用争抢；质量档用会话启动时的值）
    if session.deferred_encode && session.encode_handle.is_none() {
        recorder::start_deferred_encode(&mut session)
            .map_err(|e| format!("延迟编码启动失败: {:#}", e))?;
    }
    join(&mut session.encode_handle);

    let result = session.shared.result.lock().unwrap().take();
    // 会话对象 drop（ring 缓冲/解码器等 GB 级分配归还 Windows 堆）后收缩
    // 工作集——否则任务管理器仍显示旧峰值（用户视角"保存好了还占 3G"）
    drop(session);
    if let Err(e) = crate::memory::clean_own_working_set() {
        log::debug!("[record] 收尾工作集收缩跳过: {}", e);
    }
    match result {
        Some(Ok(r)) => Ok(serde_json::to_value(&r)
            .unwrap_or_else(|_| serde_json::json!({ "path": r.path, "frames": r.frames }))),
        Some(Err(e)) => Err(format!("录制失败: {:#}", e)),
        None => Err("编码线程未返回结果（异常终止）".into()),
    }
}

/// FINISHING 的 RAII 清除（finish_session 所有退出路径/panic 均不残留标志）
struct FinishingGuard;
impl Drop for FinishingGuard {
    fn drop(&mut self) {
        FINISHING.store(false, Ordering::Release);
    }
}

/// 退出保护等待上限：纯录制模式延迟编码实测 ~69s（30s 上限录制），留约 2.5 倍余量
const FLUSH_ON_EXIT_TIMEOUT_SECS: u64 = 180;

/// 退出保护：应用退出路径的同步收尾入口（quit_app / tray_quit / restart_elevated
/// 在 app.exit 前调用）
///
/// - 会话仍活跃（录制中 / 已到自动停止上限但尚未被取走）→ 取走并执行与
///   record_stop 等价的收尾：join 编码线程、写盘；record://done/error 由编码
///   线程写完 result 时发出，停止热键由 teardown 注销。
/// - 会话已被停止/取消取走、收尾（延迟编码可达 ~1-2 分钟）仍在后台进行 →
///   等待其完成；超时（`FLUSH_ON_EXIT_TIMEOUT_SECS`）兜底放行，避免极端情况
///   （编码线程卡死在磁盘 IO）永久卡住退出。
/// - 两者皆无 → 立即返回。
///
/// 线程环境：三处调用点均在主线程（同步命令 / 托盘菜单事件）。finish_session
/// 内 join 的三线程不依赖主线程事件循环（事件发射经事件代理非阻塞投递、窗口
/// 操作在主线程上内联执行），主线程同步等待不构成死锁；等待期间不持有任何锁。
pub fn flush_on_exit(_app: &AppHandle) {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(FLUSH_ON_EXIT_TIMEOUT_SECS);
    let mut waiting_logged = false;
    loop {
        // 1. 有会话 → 取走并同步收尾（与 stop/取消路径竞态：谁先取到谁收尾）
        match session_lock().take() {
            Some(SessionSlot::Active(session)) => {
                log::info!("[record] 退出保护：检测到活跃录制，正在保存录制…");
                if let Err(e) = finish_session(session, false) {
                    log::error!("[record] 退出保护：收尾失败（产物可能不完整）: {}", e);
                }
            }
            // 倒计时占位（会话未 spawn，无数据）：撤销沉浸式布置即可——
            // 倒计时线程醒来发现槽位已空会放弃启动
            Some(SessionSlot::Pending { app }) => {
                log::info!("[record] 退出保护：倒计时中退出，撤销沉浸式布置");
                teardown_immersive(&app);
                if let Some(w) = app.get_webview_window("record-osd") {
                    let _ = w.close();
                }
            }
            None => {}
        }
        // 2. 收尾已被其他线程接手（stop/取消已取走会话）→ 等其完成
        if !FINISHING.load(Ordering::Acquire) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            log::warn!(
                "[record] 退出保护：等待编码收尾超时（{}s），继续退出（产物可能不完整）",
                FLUSH_ON_EXIT_TIMEOUT_SECS
            );
            return;
        }
        if !waiting_logged {
            waiting_logged = true;
            log::info!("[record] 退出保护：编码收尾进行中，正在保存录制…");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
