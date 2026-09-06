//! 视频模块 Tauri 命令（对齐 record/commands.rs 成熟模式）
//!
//! - async + spawn_blocking：同步命令在主线程执行会死锁（动图模块已踩坑的教训）
//! - SESSION 槽位防重入；与动图录制全局互斥（任一在录则拒绝另一个）
//! - FINISHING 退出保护标志：视频停止收尾（排空+finalize 可达数秒）期间
//!   阻止应用退出（flush_on_exit 轮询等待）
//! - 沉浸式（热键直启）：video-osd 叠加窗（置顶/点击穿透/不入截图/不抢焦点）
//!   + 临时停止热键（默认 Alt+F8）。视频无时长上限 → 停止热键必配，
//!   注册失败即拒绝启动（fail-fast，无 30s 兜底可用）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, WebviewWindowBuilder};
use videorec::codec::Codec;
use videorec::recorder::AudioMode;

use super::session::{self, VideoResult, VideoSession, VideoStats};

/// 全局视频会话（同一时刻仅一个）
static SESSION: Mutex<Option<VideoSession>> = Mutex::new(None);

/// 退出保护：视频收尾进行中标志（flush_on_exit 轮询）
static FINISHING: AtomicBool = AtomicBool::new(false);

/// 视频录制中临时注册的停止热键（结束注销；reregister 补回）
static VIDEO_STOP_HOTKEY: Mutex<Option<crate::config::Hotkey>> = Mutex::new(None);

// ==================== 录制 ====================

/// 视频录制启动参数（前端 invoke）
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoStartOpts {
    /// 编码器：h264 | hevc
    pub codec: Option<String>,
    /// 硬编：auto | nvenc | amf | qsv | cpu
    pub hw: Option<String>,
    pub fps: Option<u32>,
    pub bitrate_mbps: Option<f64>,
    pub gop: Option<u32>,
    /// 音频：off | system | both
    pub audio: Option<String>,
    /// HDR10 直录（显示器 HDR 开启时：scRGB→P010→hevc main10 PQ 直录；
    /// 显示器非 HDR 自动回落 SDR；None = 自动判定显示器 HDR 状态）
    pub hdr: Option<bool>,
    /// 输出目录（None = 配置的录制保存目录）
    pub save_dir: Option<String>,
    /// 延迟启动（分钟；>0 = 定时器模式——立即返回，倒计时到点自动开始）
    pub delay_minutes: Option<u32>,
    /// 时长上限覆盖（秒；计划录制换算用；None = 走 [video].max_seconds）
    pub max_seconds: Option<u32>,
}

/// 开始视频录制（面板按钮路径；无沉浸式）。热键直启走 start_hotkey_triggered。
#[tauri::command]
pub async fn video_record_start(
    app: tauri::AppHandle,
    opts: VideoStartOpts,
) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        start_inner(&app, &opts, false, false)
    })
    .await
    .map_err(|e| format!("录制任务失败: {}", e))?
}

/// 录制启动核心（面板命令与热键直启共用）
///
/// osd 路径顺序：setup_video_immersive（OSD + 停止热键）→ spawn_session。
/// delay_minutes > 0 = 定时器模式（倒计时线程到点无 osd 直启）。
fn start_inner(
    app: &AppHandle,
    opts: &VideoStartOpts,
    osd: bool,
    from_hotkey: bool,
) -> Result<serde_json::Value, String> {
    // 定时器模式：延迟启动（立即返回；倒计时线程到点回调本函数 delay=None）
    if let Some(mins) = opts.delay_minutes.filter(|m| *m > 0) {
        return spawn_delayed_start(app, opts, mins);
    }
    let mut guard = session_lock();
    if guard.is_some() {
        return Err("已有视频录制进行中".into());
    }
    // 全局互斥：动图在录时拒绝视频（反向由动图侧检查）
    if crate::record::commands::is_recording_active() {
        return Err("JXL 动图录制进行中，请先停止".into());
    }
    let cfg = crate::config::Config::load();
    let codec = match opts.codec.as_deref().unwrap_or(&cfg.video.codec) {
        "hevc" => Codec::Hevc,
        _ => Codec::H264,
    };
    let audio = match opts.audio.as_deref().unwrap_or(&cfg.video.audio) {
        "system" => AudioMode::Loopback,
        "both" => AudioMode::LoopbackAndMic { mix: true },
        _ => AudioMode::Off,
    };
    let fps = opts.fps.unwrap_or(cfg.video.fps).clamp(10, 144);
    let bitrate_bps = (opts
        .bitrate_mbps
        .unwrap_or(cfg.video.bitrate_mbps)
        .max(1.0) as u64)
        * 1_000_000;
    let gop = opts.gop.unwrap_or(cfg.video.gop).clamp(1, 600);
    let hw = match opts.hw.as_deref().unwrap_or(&cfg.video.hw) {
        "nvenc" | "amf" | "qsv" | "cpu" => opts.hw.clone().unwrap(),
        _ => "auto".to_string(),
    };
    // 录制目标显示器：index 0 = 主显示器；1.. = video_list_monitors 枚举序。
    // 越界/枚举失败回落主显示器（不拒绝启动——热键路径无人看报错）
    let monitor = pick_monitor(cfg.video.monitor_index);
    // 追随鼠标模式：裁剪窗尺寸（偶数化 + 钳制 [64, 显示器] 区间）
    let follow = if cfg.video.mode == "follow" {
        let fw = (cfg.video.follow_w & !1).max(64).min(monitor.width & !1);
        let fh = (cfg.video.follow_h & !1).max(64).min(monitor.height & !1);
        Some((fw, fh))
    } else {
        None
    };
    // HDR 直录：显式请求优先；None = 自动（目标显示器 HDR 开启即 HDR10 直录）
    let hdr = match opts.hdr {
        Some(v) => v,
        None => monitor.is_hdr(),
    };

    // 输出路径：video_YYYYMMDD_HHMMSS.mkv（长录制 MKV 崩溃安全容器）
    let dir = opts
        .save_dir
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(|| cfg.recording.save_dir.clone())
        .unwrap_or_else(|| cfg.resolved_save_dir());
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建保存目录失败: {e}"))?;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = dir.join(format!("video_{ts}.mkv"));

    // 沉浸式布置（OSD + 停止热键）在会话 spawn 之前：布置失败即返回，
    // 无半启动（fail-fast：视频无时长上限，停止热键不可缺）
    let mut stop_hotkey_desc = None;
    if osd {
        stop_hotkey_desc = match setup_video_immersive(app, from_hotkey) {
            Ok(d) => d,
            Err(e) => {
                log::error!("[video] 沉浸式布置失败: {e}");
                teardown_video_immersive(app);
                destroy_video_osd(app);
                return Err(format!("沉浸式布置失败: {e}"));
            }
        };
    }

    // 会话启动（全链路前置检查，失败不留半启动；失败时撤销沉浸式布置）
    match session::spawn_session(session::VideoSpawnParams {
        output_path: path,
        monitor: monitor.clone(),
        follow,
        hdr,
        codec,
        fps,
        bitrate_bps,
        gop,
        audio,
        hw,
        max_seconds: opts.max_seconds.unwrap_or(cfg.video.max_seconds),
        max_size_bytes: cfg.video.max_size_mb.saturating_mul(1024 * 1024),
        split_size_bytes: cfg.video.split_size_gb.saturating_mul(1024 * 1024 * 1024),
        silence_stop_seconds: cfg.video.silence_stop_seconds,
        silence_countdown_seconds: cfg.video.silence_countdown_seconds,
        mouse_cursor: cfg.video.mouse_cursor,
        mouse_click: cfg.video.mouse_click,
        mouse_highlight: if cfg.video.mouse_highlight {
            Some((
                super::camera::parse_hex_color(&cfg.video.mouse_highlight_color)
                    .unwrap_or([0xFF, 0xD4, 0x00]),
                cfg.video.mouse_highlight_size.clamp(16, 300) as i32,
            ))
        } else {
            None
        },
        watermark: if !cfg.video.watermark_text.is_empty()
            || !cfg.video.watermark_image.trim().is_empty()
        {
            Some(super::watermark::WatermarkParams {
                text: cfg.video.watermark_text.clone(),
                font_size: cfg.video.watermark_font_size,
                image: cfg.video.watermark_image.trim().to_string(),
                pos: cfg.video.watermark_pos.clone(),
                opacity: cfg.video.watermark_opacity,
                margin: cfg.video.watermark_margin,
            })
        } else {
            None
        },
        camera: if !cfg.video.camera_device.trim().is_empty() {
            Some(super::camera::CameraParams {
                device: cfg.video.camera_device.trim().to_string(),
                width: cfg.video.camera_width,
                pos: cfg.video.camera_pos.clone(),
                flip_h: cfg.video.camera_flip_h,
                chroma_key: cfg.video.camera_chroma_key,
                key_color: cfg.video.camera_key_color.clone(),
                similarity: cfg.video.camera_similarity,
                margin: cfg.video.camera_margin,
            })
        } else {
            None
        },
        app: app.clone(),
    }) {
        Ok(mut s) => {
            if osd {
                s.osd_app = Some(app.clone());
            }
            let info = serde_json::json!({
                "path": s.output_path.to_string_lossy(),
                "fps": fps,
                "encoder": s.encoder,
                "audio": opts.audio.clone().unwrap_or_else(|| cfg.video.audio.clone()),
                "stopHotkey": stop_hotkey_desc,
            });
            *guard = Some(s);
            Ok(info)
        }
        Err(e) => {
            if osd {
                // 撤销布置 + 直接销毁 OSD（此时 OSD 前端可能尚未挂载完监听，
                // 靠事件驱动关闭有竞态——强制关窗最可靠）
                teardown_video_immersive(app);
                destroy_video_osd(app);
            }
            Err(e)
        }
    }
}

/// 停止录制并等待 finalize（阻塞至 MKV 写完；nvenc 排空 + Cues 回填秒级）
#[tauri::command]
pub async fn video_record_stop() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(stop_blocking)
        .await
        .map_err(|e| format!("停止任务失败: {}", e))?
}

fn stop_blocking() -> Result<serde_json::Value, String> {
    let s = {
        let mut guard = session_lock();
        // 在持锁时置 FINISHING（happens-before：flush_on_exit 不会漏等）；
        // 仅 take 成功才置——take 失败（无会话）时置了没人复位 = 永久锁死互斥
        match guard.take() {
            Some(s) => {
                FINISHING.store(true, Ordering::Release);
                s
            }
            None => return Err("没有进行中的视频录制".into()),
        }
    };
    let _finishing = FinishingGuard;
    finish_session(s, true)
}

/// 取消录制（丢弃半成品并删除文件）
#[tauri::command]
pub async fn video_record_cancel() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let s = {
            let mut guard = session_lock();
            match guard.take() {
                Some(s) => {
                    FINISHING.store(true, Ordering::Release);
                    s
                }
                None => return Err("没有进行中的视频录制".into()),
            }
        };
        let _finishing = FinishingGuard;
        s.stop.store(true, Ordering::Release);
        if let Some(app) = s.osd_app.as_ref() {
            teardown_video_immersive(app);
        }
        if let Some(h) = s.handle {
            let _ = h.join();
        }
        // 删除全部分卷（含首文件；录制线程 push，无竞态——线程已 join）
        let parts = s.parts.lock().unwrap_or_else(|p| p.into_inner()).clone();
        for p in &parts {
            let _ = std::fs::remove_file(p);
        }
        log::info!(
            "[video] 已取消，半成品已删除（{} 个分卷）: {}",
            parts.len(),
            s.output_path.display()
        );
        Ok(())
    })
    .await
    .map_err(|e| format!("取消任务失败: {}", e))?
}

/// 录制状态查询（前端面板轮询）
#[tauri::command]
pub fn video_record_status() -> serde_json::Value {
    let guard = session_lock();
    match guard.as_ref() {
        Some(s) => {
            let st = s.stats.lock().unwrap().clone();
            serde_json::json!({ "recording": true, "stats": st })
        }
        None => serde_json::json!({ "recording": false }),
    }
}

/// 暂停录制（时间戳经会话 MediaClock 折叠；音频暂停区间块自动丢弃）
#[tauri::command]
pub fn video_record_pause() -> Result<(), String> {
    let guard = session_lock();
    match guard.as_ref() {
        Some(s) => {
            if s.pause.swap(true, Ordering::AcqRel) {
                return Err("录制已处于暂停状态".into());
            }
            log::info!("[video] video_record_pause 命令");
            Ok(())
        }
        None => Err("没有进行中的视频录制".into()),
    }
}

/// 恢复录制（暂停后的时间轴无缝续接）
#[tauri::command]
pub fn video_record_resume() -> Result<(), String> {
    let guard = session_lock();
    match guard.as_ref() {
        Some(s) => {
            if !s.pause.swap(false, Ordering::AcqRel) {
                return Err("录制未处于暂停状态".into());
            }
            log::info!("[video] video_record_resume 命令");
            Ok(())
        }
        None => Err("没有进行中的视频录制".into()),
    }
}

/// 视频会话是否活跃（动图侧互斥检查用 + 退出保护）
pub fn is_video_recording() -> bool {
    session_lock().is_some() || FINISHING.load(Ordering::Acquire)
}

/// 开始视频录制热键（默认 Alt+F7）：游戏模式直启全屏 MKV 录制 + 沉浸式
///
/// 语义对齐动图 start_hotkey_triggered：不最小化主面板（游戏全屏覆盖，
/// minimize 扰动 DWM = 黑屏闪动 + 焦点被夺）。
/// **录制进行中再按 = 暂停/恢复切换**（Bandicam 同键习惯）。
/// 失败（DLL 缺失/编码器不可用/停止热键注册失败）经 video://error 事件
/// 反馈（热键路径无面板交互）。
pub fn start_hotkey_triggered(app: &AppHandle) {
    // 已有会话 → Alt+F7 = 暂停/恢复切换（Bandicam 同键习惯；游戏全屏无面板可点）
    {
        let guard = session_lock();
        if let Some(s) = guard.as_ref() {
            let now_paused = !s.pause.load(Ordering::Acquire);
            s.pause.store(now_paused, Ordering::Release);
            log::info!("[video] Alt+F7 热键：{}", if now_paused { "暂停" } else { "恢复" });
            return;
        }
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("video-start-hotkey".into())
        .spawn(move || {
            // 游戏模式直启：osd=true；from_hotkey=true 跳过面板最小化
            let opts = VideoStartOpts {
                codec: None,
                hw: None,
                fps: None,
                bitrate_mbps: None,
                gop: None,
                audio: None,
                hdr: None,
                save_dir: None,
                delay_minutes: None,
                max_seconds: None,
            };
            match start_inner(&app, &opts, true, true) {
                Ok(_) => {
                    // 面板同步 recording 态（面板可见时；OSD 在屏时面板状态无感）
                    let _ = app.emit("video://hotkey-started", ());
                }
                Err(e) => {
                    log::error!("[video] 热键启动视频录制失败: {e}");
                    let _ = app.emit("video://error", e);
                }
            }
        })
        .ok();
}

/// 停止视频热键入口（on_global_shortcut 调用；后台线程执行——finalize
/// 秒级但不可阻塞热键分发线程）
pub fn stop_hotkey_triggered(app: &AppHandle) {
    let s = {
        let mut guard = session_lock();
        match guard.take() {
            Some(s) => {
                FINISHING.store(true, Ordering::Release);
                s
            }
            None => return, // 无会话容错忽略
        }
    };
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("video-stop-hotkey".into())
        .spawn(move || {
            let _finishing = FinishingGuard; // 线程结束复位（flush_on_exit 依赖）
            // OSD 切"保存中"状态（立即反馈）
            let _ = app.emit("video://stopping", ());
            match finish_session(s, true) {
                Ok(_) => {}
                Err(e) => {
                    let _ = app.emit("video://error", e);
                }
            }
        });
    if spawned.is_err() {
        // 线程创建失败：会话已取走但无人收尾——置回标志让后续 stop/flush 兜底
        FINISHING.store(false, Ordering::Release);
    }
}

/// 录制线程自动停止（时长/大小阈值）后的槽位清理 + 沉浸式收尾。
/// 由录制线程自身在 finalize + done 事件之后调用（handle 不可 join 自己）；
/// 与 stop/cancel/flush_on_exit 双收尾竞态安全：take 为 None = 对方已接管。
pub fn auto_stopped_cleanup(app: &AppHandle) {
    let taken = session_lock().take();
    if let Some(mut s) = taken {
        FINISHING.store(true, Ordering::Release);
        s.handle = None; // 录制线程自身调用：不 join（JoinHandle drop = detach，线程即将退出）
        let was_osd = s.osd_app.is_some();
        if let Some(app2) = s.osd_app.take() {
            teardown_video_immersive(&app2);
        }
        FINISHING.store(false, Ordering::Release);
        log::info!("[video] 自动停止收尾完成（槽位已清理）");
        // 完成动作（exit/shutdown 在此线程执行即时生效；new spawn 重启线程）
        execute_complete_action(app, was_osd);
    }
}

/// 退出保护入口（lib.rs 的 quit 路径调用；对齐 record::flush_on_exit）
pub fn flush_on_exit() {
    loop {
        let taken = {
            let mut guard = session_lock();
            if guard.is_some() {
                FINISHING.store(true, Ordering::Release);
                guard.take()
            } else {
                None
            }
        };
        if let Some(s) = taken {
            let _ = finish_session(s, false);
            // 自己置的标志自己复位（finish_session 不动 FINISHING；
            // 不复位 = 下方轮询永真 → 退出路径死循环）
            FINISHING.store(false, Ordering::Release);
            continue;
        }
        if !FINISHING.load(Ordering::Acquire) {
            return; // 无会话且无收尾进行中
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// 结束会话：置停 → join 线程（线程内完成排空 + finalize + done 事件）
/// → teardown 沉浸式 → 返回结果。
/// run_action = 成功收尾后执行录制完成动作（手动/热键/自动停止路径 true；
/// flush_on_exit 退出保护路径 false——应用即将退出不触发）
fn finish_session(mut s: VideoSession, run_action: bool) -> Result<serde_json::Value, String> {
    s.stop.store(true, Ordering::Release);
    if let Some(h) = s.handle.take() {
        let _ = h.join();
    }
    // 沉浸式收尾（注销停止热键 + 停置顶看护 + 还原面板）——finalize 完成后
    //（对齐动图 finish_slot 顺序；OSD 窗本体由前端 done 展示后自毁）
    let was_osd = s.osd_app.is_some();
    if let Some(app) = s.osd_app.take() {
        teardown_video_immersive(&app);
    }
    let r = s
        .result
        .lock()
        .unwrap()
        .take()
        .ok_or("视频会话未返回结果（异常终止）")?;
    if run_action {
        execute_complete_action(&s.app, was_osd);
    }
    Ok(serde_json::to_value(&r).unwrap_or_else(|_| {
        serde_json::json!({ "path": r.path })
    }))
}

fn session_lock() -> std::sync::MutexGuard<'static, Option<VideoSession>> {
    SESSION
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// 按索引选录制目标显示器：0 = 主显示器（left=0&&top=0 约定）；1.. =
/// video_list_monitors 枚举序（index 1 = 枚举第一个显示器）。越界/枚举
/// 失败回落主显示器，再退首个。
fn pick_monitor(index: u32) -> crate::capture::monitor::MonitorInfo {
    let monitors = crate::capture::monitor::enumerate_monitors()
        .map_err(|e| {
            log::warn!("[video] 显示器枚举失败: {e}");
            e
        })
        .unwrap_or_default();
    let pick = if index == 0 {
        None
    } else {
        monitors.get(index as usize - 1)
    };
    let m = pick
        .cloned()
        .or_else(|| {
            monitors
                .iter()
                .find(|m| m.left == 0 && m.top == 0 && m.attached_to_desktop)
                .cloned()
        })
        .or_else(|| monitors.first().cloned());
    match m {
        Some(m) => {
            if index != 0 && monitors.get(index as usize - 1).is_none() {
                log::warn!(
                    "[video] 显示器索引 {index} 不存在（共 {} 个），回落主显示器",
                    monitors.len()
                );
            }
            m
        }
        None => crate::capture::monitor::MonitorInfo {
            // 终极兜底：枚举全失败——虚构主屏参数让 DDA 报错路径接管
            adapter_index: 0,
            output_index: 0,
            adapter_name: String::new(),
            device_name: "\\\\.\\DISPLAY1".to_string(),
            width: 1920,
            height: 1080,
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
            bits_per_color: 8,
            hdr_mode: crate::capture::monitor::HdrMode::Sdr,
            sdr_white_level_nits: 80.0,
            max_full_frame_luminance: 0.0,
            max_luminance: 0.0,
            attached_to_desktop: true,
            hmonitor: 0,
            gamut_r: (0.0, 0.0),
            gamut_g: (0.0, 0.0),
            gamut_b: (0.0, 0.0),
            gamut_white: (0.0, 0.0),
            gamut_valid: false,
            edid: None,
        },
    }
}

/// 显示器列表（设置页下拉）：index 与 [video].monitor_index 对应（0 = 主显示器）
#[tauri::command]
pub fn video_list_monitors() -> serde_json::Value {
    let monitors = crate::capture::monitor::enumerate_monitors().unwrap_or_default();
    let list: Vec<serde_json::Value> = std::iter::once(serde_json::json!({
        // index 0 恒为主显示器占位（pick_monitor 语义）
        "index": 0,
        "label": format!("主显示器（{}）", primary_label(&monitors)),
    }))
    .chain(monitors.iter().enumerate().map(|(i, m)| {
        serde_json::json!({
            "index": i + 1,
            "label": format!(
                "显示器 {}（{}x{}{}）",
                i + 1,
                m.width,
                m.height,
                if m.is_hdr() { " HDR" } else { "" },
            ),
        })
    }))
    .collect();
    serde_json::json!({ "monitors": list })
}

fn primary_label(monitors: &[crate::capture::monitor::MonitorInfo]) -> String {
    monitors
        .iter()
        .find(|m| m.left == 0 && m.top == 0 && m.attached_to_desktop)
        .or_else(|| monitors.first())
        .map(|m| format!("{}x{}", m.width, m.height))
        .unwrap_or_else(|| "未知".to_string())
}

/// 摄像头设备列表（设置页下拉）：id = symbolic link（[video].camera_device 持久化值）
#[tauri::command]
pub async fn video_list_cameras() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(super::camera::enumerate_cameras)
        .await
        .map(|cams| {
            serde_json::json!({
                "cameras": cams
                    .iter()
                    .map(|c| serde_json::json!({ "id": c.id, "label": c.label }))
                    .collect::<Vec<_>>(),
            })
        })
        .map_err(|e| format!("摄像头枚举任务失败: {e}"))
}

/// FINISHING 的 RAII 清除（panic 安全；显式复位后 Drop 再复位无害）
struct FinishingGuard;
impl Drop for FinishingGuard {
    fn drop(&mut self) {
        FINISHING.store(false, Ordering::Release);
    }
}

// ==================== 录制完成动作 ====================

/// 执行录制完成动作（成功收尾后调用；cancel 取消路径不触发）。
/// was_osd = 会话是否沉浸式（热键直启）——"new" 重启时保持同语义。
pub fn execute_complete_action(app: &AppHandle, was_osd: bool) {
    let cfg = crate::config::Config::load().video;
    let action = cfg.complete_action.as_str();
    if action.is_empty() || action == "none" {
        return;
    }
    log::info!("[video] 录制完成动作: {action}");
    match action {
        "new" => {
            // 重启录制：spawn 线程（槽位清理竞态安全——sleep 等待收尾方退出锁）
            let app2 = app.clone();
            std::thread::Builder::new()
                .name("video-restart".into())
                .spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                    let opts = VideoStartOpts {
                        codec: None,
                        hw: None,
                        fps: None,
                        bitrate_mbps: None,
                        gop: None,
                        audio: None,
                        hdr: None,
                        save_dir: None,
                        delay_minutes: None,
                        max_seconds: None,
                    };
                    match start_inner(&app2, &opts, was_osd, was_osd) {
                        Ok(_) => {
                            let _ = app2.emit("video://restarted", ());
                        }
                        Err(e) => {
                            log::error!("[video] 录新视频失败: {e}");
                            let _ = app2.emit("video://error", e);
                        }
                    }
                })
                .ok();
        }
        "exit" => {
            let _ = crate::quit_app_path(app.clone());
        }
        "shutdown" => {
            let secs = cfg.shutdown_countdown_seconds.clamp(5, 600);
            match std::process::Command::new("shutdown")
                .args(["/s", "/t", &secs.to_string()])
                .spawn()
            {
                Ok(_) => {
                    log::info!("[video] 系统将在 {secs}s 后关机（可取消）");
                    let _ = app.emit("video://shutdown-countdown", secs);
                }
                Err(e) => {
                    log::error!("[video] 关机命令失败: {e}");
                    let _ = app.emit("video://error", format!("关机命令失败: {e}"));
                }
            }
        }
        _ => log::warn!("[video] 未知完成动作: {action}"),
    }
}

/// 取消关机（前端倒计时提示的取消按钮；shutdown /a 幂等——无计划关机时无害）
#[tauri::command]
pub fn video_cancel_shutdown() -> Result<bool, String> {
    let ok = std::process::Command::new("shutdown")
        .arg("/a")
        .spawn()
        .map(|_| true)
        .map_err(|e| format!("取消关机失败: {e}"))?;
    if ok {
        log::info!("[video] 已取消计划关机");
    }
    Ok(ok)
}

// ==================== 沉浸式（热键 OSD） ====================

/// 沉浸式布置：OSD 叠加窗 + 临时停止热键。
///
/// 与动图 setup_immersive 的差异：
/// - 热键路径恒不最小化主面板（视频热键唯一入口即热键，无面板路径 OSD）
/// - 停止热键 fail-fast：视频无时长上限，无法靠 autostop 兜底 →
///   配置缺失/注册失败直接 Err（拒绝启动），而非降级提示
fn setup_video_immersive(app: &AppHandle, from_hotkey: bool) -> Result<Option<String>, String> {
    // 1. 主面板最小化——仅非热键路径（面板可见即将被录进画面）。
    //    热键路径跳过（游戏全屏覆盖，minimize 扰动 DWM = 黑屏闪动根因）
    if !from_hotkey {
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.minimize();
        }
    }

    // 2. OSD 叠加窗（存在则复用）
    //
    // 崩溃教训（对齐动图）：Tauri v2 窗口创建必须在主线程——
    // run_on_main_thread 转发 + mpsc 等待结果
    if app.get_webview_window("video-osd").is_none() {
        let app2 = app.clone();
        let result: std::sync::mpsc::Receiver<Result<(), String>> = {
            let (tx, rx) = std::sync::mpsc::channel();
            app.run_on_main_thread(move || {
                let r = (|| -> Result<(), String> {
                    let window = WebviewWindowBuilder::new(
                        &app2,
                        "video-osd",
                        tauri::WebviewUrl::App("#/video-osd".into()),
                    )
                    .title("")
                    .decorations(false)
                    .transparent(true)
                    .shadow(false)
                    .always_on_top(true)
                    .skip_taskbar(true)
                    .resizable(false)
                    // 先不可见创建：build→样式设置→SW_SHOWNOACTIVATE（防激活竞窗）
                    .visible(false)
                    .focused(false)
                    .inner_size(260.0, 44.0)
                    .position(9999.0, 24.0) // 屏幕外，后端定位到主屏右上
                    .build()
                    .map_err(|e| format!("创建视频 OSD 窗口失败: {}", e))?;

                    if let Ok(hwnd) = window.hwnd() {
                        let hwnd_raw = hwnd.0 as isize;
                        let hwnd =
                            windows::Win32::Foundation::HWND(hwnd_raw as *mut core::ffi::c_void);
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
                                    | windows::Win32::UI::WindowsAndMessaging::WS_EX_TRANSPARENT.0
                                        as isize
                                    | windows::Win32::UI::WindowsAndMessaging::WS_EX_LAYERED.0
                                        as isize
                                    | windows::Win32::UI::WindowsAndMessaging::WS_EX_NOACTIVATE.0
                                        as isize,
                            );
                        }
                        // 不入截图/录屏
                        crate::set_window_display_affinity(hwnd_raw, true);
                        unsafe {
                            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                                hwnd,
                                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
                            );
                        }
                        // 置顶看护（全屏游戏获焦会把全屏窗提到 topmost 之上）
                        start_video_osd_keep_top(hwnd_raw);
                    }

                    // 位置兜底：主屏右上角
                    match window.primary_monitor() {
                        Ok(Some(mon)) => {
                            let size = window
                                .outer_size()
                                .unwrap_or(tauri::PhysicalSize::new(260, 44));
                            let margin = (16.0 * mon.scale_factor()).round() as i32;
                            let x = mon.position().x + mon.size().width as i32
                                - size.width as i32
                                - margin;
                            let y = mon.position().y + margin;
                            let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
                        }
                        _ => log::warn!("[video] OSD 定位: primary_monitor 不可用"),
                    }
                    Ok(())
                })();
                let _ = tx.send(r);
            })
            .map_err(|e| format!("主线程队列不可用，无法创建视频 OSD 窗口: {}", e))?;
            rx
        };
        result
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| format!("视频 OSD 窗口创建超时: {}", e))??;
    }

    // 3. 停止热键（fail-fast：视频无时长上限，无兜底可用）
    let hk = crate::config::Config::load().video_stop_hotkey;
    let Some(hk) = hk else {
        stop_video_osd_keep_top();
        return Err("未配置视频停止热键（设置 → 热键），无法安全启动游戏模式录制".into());
    };
    let desc = crate::hotkey_display_name(&hk);
    if !crate::try_register_stop_hotkey(app, hk) {
        stop_video_osd_keep_top();
        return Err(format!(
            "停止热键注册失败（{}，可能被占用），拒绝启动",
            desc.unwrap_or_default()
        ));
    }
    *VIDEO_STOP_HOTKEY.lock().unwrap() = Some(hk);
    Ok(desc)
}

/// 沉浸式收尾：注销停止热键 + 停置顶看护 + 还原主面板（仅我们最小化过的）
fn teardown_video_immersive(app: &AppHandle) {
    let hk = VIDEO_STOP_HOTKEY.lock().unwrap().take();
    if let Some(hk) = hk {
        crate::try_unregister_stop_hotkey(app, hk);
    }
    stop_video_osd_keep_top();
    // 主面板还原：仅面板路径（is_minimized 恒 true）；热键路径未动过面板
    if let Some(main) = app.get_webview_window("main") {
        if main.is_minimized().unwrap_or(false) {
            let _ = main.unminimize();
            let _ = main.show();
        }
    }
}

/// 录制中补注册停止热键（reregister_all_hotkeys 的 unregister_all 会清掉它：
/// 配置保存/热键录入结束都会走到那里；此函数仅在会话活跃时补回）
pub fn reregister_video_stop_hotkey_if_active(app: &AppHandle) {
    if session_lock().is_none() {
        return;
    }
    if let Some(hk) = VIDEO_STOP_HOTKEY.lock().unwrap().clone() {
        crate::try_register_stop_hotkey(app, hk);
    }
}

/// OSD 自毁（前端 done/error 展示后调用）
#[tauri::command]
pub fn close_video_osd(app: AppHandle) -> Result<(), String> {
    stop_video_osd_keep_top();
    if let Some(w) = app.get_webview_window("video-osd") {
        let _ = w.close();
    }
    Ok(())
}

/// 强制销毁 OSD 窗（启动失败路径：OSD 前端监听可能未挂载，事件驱动关闭
/// 有竞态——直接关窗最可靠）
fn destroy_video_osd(app: &AppHandle) {
    stop_video_osd_keep_top();
    if let Some(w) = app.get_webview_window("video-osd") {
        let _ = w.close();
    }
}

// ==================== OSD 置顶看护 ====================

/// OSD 置顶看护开关（true = 看护线程运行中）
static VIDEO_OSD_KEEP_TOP: AtomicBool = AtomicBool::new(false);

/// 启动 OSD 置顶看护线程：每 500ms 重申 HWND_TOPMOST（SWP_NOACTIVATE 不抢焦点）
fn start_video_osd_keep_top(hwnd_raw: isize) {
    VIDEO_OSD_KEEP_TOP.store(true, Ordering::Release);
    std::thread::Builder::new()
        .name("video-osd-keep-top".into())
        .spawn(move || {
            while VIDEO_OSD_KEEP_TOP.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if !VIDEO_OSD_KEEP_TOP.load(Ordering::Acquire) {
                    break;
                }
                let hwnd =
                    windows::Win32::Foundation::HWND(hwnd_raw as *mut core::ffi::c_void);
                unsafe {
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

fn stop_video_osd_keep_top() {
    VIDEO_OSD_KEEP_TOP.store(false, Ordering::Release);
}

// ==================== 定时器（延迟启动）+ 计划录制调度器 ====================

/// 延迟启动取消标志（true = 放弃倒计时；每次启动重置）
static DELAY_CANCEL: AtomicBool = AtomicBool::new(false);

/// 定时器模式：spawn 倒计时线程（每秒 video://countdown；到点直启录制）。
/// 立即返回提示——倒计时中可 video_cancel_delayed_start 取消。
fn spawn_delayed_start(
    app: &AppHandle,
    opts: &VideoStartOpts,
    mins: u32,
) -> Result<serde_json::Value, String> {
    // 已有延迟线程在跑 → 拒绝（单一延迟槽）
    if DELAY_ACTIVE.load(Ordering::Acquire) {
        return Err("已有延迟启动倒计时进行中".into());
    }
    DELAY_CANCEL.store(false, Ordering::Release);
    DELAY_ACTIVE.store(true, Ordering::Release);
    let app2 = app.clone();
    // 延迟到点用无 osd 面板语义 + 保留 max_seconds 覆盖
    let mut opts2 = opts.clone();
    opts2.delay_minutes = None;
    let secs = mins as u64 * 60;
    std::thread::Builder::new()
        .name("video-delay-start".into())
        .spawn(move || {
            for remain in (1..=secs).rev() {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if DELAY_CANCEL.load(Ordering::Acquire) {
                    let _ = app2.emit("video://countdown", 0u64);
                    log::info!("[video] 延迟启动已取消");
                    DELAY_ACTIVE.store(false, Ordering::Release);
                    return;
                }
                let _ = app2.emit("video://countdown", remain);
            }
            DELAY_ACTIVE.store(false, Ordering::Release);
            match start_inner(&app2, &opts2, false, false) {
                Ok(_) => {
                    log::info!("[video] 延迟 {} 分钟后录制已开始", mins);
                    // 面板同步录制态（复用热键启动事件——前端 listener 相同）
                    let _ = app2.emit("video://hotkey-started", ());
                }
                Err(e) => {
                    log::error!("[video] 延迟启动失败: {e}");
                    let _ = app2.emit("video://error", e);
                }
            }
        })
        .map_err(|e| format!("延迟启动线程创建失败: {e}"))?;
    log::info!("[video] 定时器：{mins} 分钟后自动开始录制");
    Ok(serde_json::json!({ "delayed": true, "minutes": mins }))
}

/// 取消延迟启动（倒计时中调用；无延迟任务时无害忽略）
#[tauri::command]
pub fn video_cancel_delayed_start() -> Result<bool, String> {
    let was = DELAY_ACTIVE.load(Ordering::Acquire);
    if was {
        DELAY_CANCEL.store(true, Ordering::Release);
    }
    Ok(was)
}

/// 延迟线程活跃标志（重复启动拒绝 + 取消语义判定）
static DELAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 计划录制调度线程启动（lib.rs setup 时调用一次；幂等）。
/// 每 30s 轮询 config：到达计划开始时刻且无会话 → 自动开始（end 换算
/// max_seconds 复用自动停止）。once 触发后自动禁用并回写 config。
pub fn start_scheduler(app: &AppHandle) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("video-scheduler".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            schedule_tick(&app);
        })
        .ok();
}

/// 单次调度判定（独立函数便于测试语义）
fn schedule_tick(app: &AppHandle) {
    use chrono::{Datelike, Timelike};
    let cfg = crate::config::Config::load();
    let v = &cfg.video;
    if !v.schedule_enabled {
        return;
    }
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    if v.schedule_last_fired == today {
        return; // 今天已触发（daily/weekly 防重）
    }
    // 周匹配（weekly）
    if v.schedule_repeat == "weekly" {
        let wd = now.weekday().number_from_monday();
        let hit = v
            .schedule_weekdays
            .split(',')
            .filter_map(|s| s.trim().parse::<u32>().ok())
            .any(|d| d == wd);
        if !hit {
            return;
        }
    } else if !matches!(v.schedule_repeat.as_str(), "once" | "daily") {
        return;
    }
    // 时刻匹配：now ∈ [start, start+30min)（30s 轮询天然命中；宽限窗
    // 允许刚开机错过几分钟的场景补触发）
    let Some((sh, sm)) = parse_hhmm(&v.schedule_start) else {
        return;
    };
    let now_min = now.hour() * 60 + now.minute();
    let start_min = sh * 60 + sm;
    if now_min < start_min || now_min >= start_min + 30 {
        return;
    }
    // 会话占用（视频在录/延迟启动中/动图在录）→ 不标记已触发：
    // 录制结束后下一 tick 宽限窗内可补触发
    if session_lock().is_some()
        || DELAY_ACTIVE.load(Ordering::Acquire)
        || crate::record::commands::is_recording_active()
    {
        return;
    }
    // 时长 = end − start（跨天：end ≤ start → +24h）
    let (eh, em) = parse_hhmm(&v.schedule_end).unwrap_or((sh, sm));
    let end_min = eh * 60 + em;
    let dur_min = if end_min > start_min {
        end_min - start_min
    } else {
        end_min + 1440 - start_min
    };
    // 标记已触发 + once 自动禁用（先回写防线程重入）
    let mut cfg2 = cfg.clone();
    cfg2.video.schedule_last_fired = today;
    if cfg2.video.schedule_repeat == "once" {
        cfg2.video.schedule_enabled = false;
    }
    if let Err(e) = cfg2.save() {
        log::warn!("[video] 计划触发状态回写失败: {e}");
        return;
    }
    log::info!(
        "[video] 计划录制触发（{} {start_min} 分钟起，时长 {dur_min} 分钟）",
        v.schedule_repeat
    );
    let opts = VideoStartOpts {
        codec: None,
        hw: None,
        fps: None,
        bitrate_mbps: None,
        gop: None,
        audio: None,
        hdr: None,
        save_dir: None,
        delay_minutes: None,
        max_seconds: Some(dur_min * 60),
    };
    match start_inner(app, &opts, false, false) {
        Ok(_) => {
            let _ = app.emit("video://hotkey-started", ()); // 面板同步录制态
        }
        Err(e) => {
            log::error!("[video] 计划录制启动失败: {e}");
            let _ = app.emit("video://error", e);
        }
    }
}

/// "HH:MM" → (时, 分)
fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h < 24 && m < 60 {
        Some((h, m))
    } else {
        None
    }
}

// ==================== 播放器（P0 独立窗口） ====================

/// 播放器启动参数
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoPlayOpts {
    /// 起始跳转（秒）
    pub seek: Option<f64>,
    pub volume: Option<f32>,
    /// 倍速
    pub rate: Option<f64>,
    /// 无边框（默认 true）
    pub borderless: Option<bool>,
    /// 播完自动关窗（单文件默认 true）
    pub auto_exit: Option<bool>,
    /// 显式字幕文件
    pub subtitle: Option<String>,
}

/// 打开播放器（P0 独立原生窗口；PlayerWindow 自持消息循环）
#[tauri::command]
pub async fn video_play_open(
    paths: Vec<String>,
    opts: VideoPlayOpts,
) -> Result<serde_json::Value, String> {
    let files: Vec<std::path::PathBuf> = paths
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    if files.is_empty() {
        return Err("播放列表为空".into());
    }
    super::player::open_player(files, opts)?;
    Ok(serde_json::json!({ "opened": paths.len() }))
}

/// 关闭播放器（P0：向窗口发 WM_CLOSE；未开则忽略）
#[tauri::command]
pub fn video_play_close() -> Result<(), String> {
    super::player::close_player();
    Ok(())
}

// ==================== V18 画质增强（FX 参数） ====================

/// 播放器画质增强状态（V18；前端卡片拉取：config 持久值 + 播放器运行态）
#[tauri::command]
pub fn video_play_fx_status() -> serde_json::Value {
    let cfg = crate::config::Config::load().video;
    serde_json::json!({
        "running": super::player::player_alive(),
        "shader": cfg.player_fx_shader,
        "params": cfg.player_fx_params,
    })
}

/// 设置画质增强 FX（V18；shader="OFF" 关闭；params 8 槽位——
/// 实时 PostMessage 生效（未运行仅持久化），下次开播放器按 config 恢复）
#[tauri::command]
pub fn video_play_fx_set(
    shader: Option<String>,
    params: Option<Vec<f32>>,
) -> Result<(), String> {
    // 持久化（config.toml [video] player_fx_*）
    let mut cfg = crate::config::Config::load();
    if let Some(s) = &shader {
        cfg.video.player_fx_shader = s.clone();
    }
    if let Some(p) = &params {
        let mut a = cfg.video.player_fx_params;
        for (i, &v) in p.iter().enumerate().take(8) {
            a[i] = v;
        }
        cfg.video.player_fx_params = a;
    }
    cfg.save().map_err(|e| format!("配置保存失败: {e}"))?;
    // 实时生效（播放器未运行 = 仅持久化，不报错）
    if let Some(s) = &shader {
        let target = if s == "OFF" { None } else { Some(s.as_str()) };
        super::player::send_fx_select(target).ok();
    }
    if let Some(p) = params {
        let mut a = [0f32; 8];
        for (i, &v) in p.iter().enumerate().take(8) {
            a[i] = v;
        }
        super::player::send_fx_params(a).ok();
    }
    Ok(())
}

/// FX 参数复位（V18；播放器实时——按当前 FX 元数据 default；config 不变）
#[tauri::command]
pub fn video_play_fx_reset() -> Result<(), String> {
    super::player::send_fx_reset()
}

// ==================== 组件状态 ====================

/// FFmpeg DLL 探测结果（设置页显示「视频组件：已就绪」）
#[tauri::command]
pub fn video_component_status() -> serde_json::Value {
    match videorec::ffmpeg::FFmpegLib::probe() {
        Ok(lib) => serde_json::json!({
            "available": true,
            "version": lib.version_string(),
        }),
        Err(e) => serde_json::json!({
            "available": false,
            "message": e,
        }),
    }
}

// re-export（编译期类型使用检查）
#[allow(unused)]
fn _type_check(_: &VideoStats, _: &VideoResult) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hhmm_parsing() {
        assert_eq!(parse_hhmm("09:00"), Some((9, 0)));
        assert_eq!(parse_hhmm("23:59"), Some((23, 59)));
        assert_eq!(parse_hhmm("0:5"), Some((0, 5)));
        assert_eq!(parse_hhmm("24:00"), None, "时越界");
        assert_eq!(parse_hhmm("09:60"), None, "分越界");
        assert_eq!(parse_hhmm("9点"), None);
        assert_eq!(parse_hhmm(""), None);
    }
}
