//! 播放器宿主（独立原生窗口）
//!
//! videorec PlayerWindow 自持 Win32 消息循环（RegisterClassW
//! "videorec_player"）——独立线程 spawn 后 run() 阻塞至窗口关闭。
//! 关闭：PostMessage WM_CLOSE（视频窗口线程，跨进程安全同进程亦安全）。
//!
//! 键位/字幕/硬解/倍速/AB/章节/书签等 30+ 功能 videorec 内置，无需前端。
//!
//! （历史：S5 曾有 viewer 标签页嵌入模式（embed_owner 挂看图窗口下），
//! 因共用窗口体验不佳已移除——视频一律独立窗口，PotPlayer 形态。）

use std::path::PathBuf;
use std::sync::Mutex;

use videorec::player::{
    AudioOut, D3d11Renderer, MpcVrRenderer, PlayerConfig, PlayerEngine, PlayerWindow, Renderer,
    WasapiRender,
};

use super::commands::VideoPlayOpts;

/// 播放器窗口 HWND（0 = 未开）；video_play_close 的目标
static PLAYER_HWND: Mutex<Option<isize>> = Mutex::new(None);

/// 播放器窗口存活（启动即播放模式的应用生命周期判定）
pub fn player_alive() -> bool {
    PLAYER_HWND.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// 打开播放器（多文件播放列表；工厂按需建渲染/音频输出，切文件复用）
pub fn open_player(paths: Vec<PathBuf>, opts: VideoPlayOpts) -> Result<(), String> {
    log::info!("[video] open_player 进入（{} 个文件）", paths.len());
    // FFmpeg DLL 前置（播放解码必需）
    let lib = std::sync::Arc::new(videorec::ffmpeg::FFmpegLib::probe().map_err(|e| {
        format!("视频组件不可用：{e}（需在 exe 目录 ffmpeg\\bin 放置 FFmpeg DLL）")
    })?);
    log::info!("[video] FFmpeg probe 完成");

    let single = paths.len() == 1;
    // V18 画质增强启动状态（config.toml [video] player_fx_*）
    let fx = crate::config::Config::load().video;
    // V18.6 渲染后端选择（"mpcvr" = MPC Video Renderer；失败自动回退内置）
    let use_mpcvr = fx.player_renderer.eq_ignore_ascii_case("mpcvr");
    let cfg = PlayerConfig {
        borderless: opts.borderless.unwrap_or(true),
        volume: opts.volume.unwrap_or(1.0).clamp(0.0, 1.0),
        auto_exit_on_end: opts.auto_exit.unwrap_or(single),
        files: paths,
        resume: true, // 播放位置记忆（videorec history）
        subtitle: opts.subtitle.as_ref().map(PathBuf::from),
        embed_owner: None, // 独立窗口模式
        fx_shader: Some(fx.player_fx_shader.clone()),
        fx_params: Some(fx.player_fx_params),
    };
    let seek = opts.seek.unwrap_or(0.0);
    let rate = opts.rate.unwrap_or(1.0);

    let lib2 = lib.clone();
    std::thread::Builder::new()
        .name("jietu-video-player".into())
        // 8MB 栈：默认 2MB 下渲染器着色器热加载/字幕解析的深层调用链
        // 可能溢出——溢出走 abort（不进 panic hook）= 无声死亡
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let mut win = match PlayerWindow::open(cfg, move |path, hwnd, outs| {
                let (r, a): (Box<dyn Renderer>, Box<dyn AudioOut>) = match outs {
                    Some(x) => x,
                    None => {
                        // V18.6：MPCVR 后端（失败回退内置 D3D11）
                        let renderer: Box<dyn Renderer> = if use_mpcvr {
                            match MpcVrRenderer::new(hwnd) {
                                Ok(r) => {
                                    log::info!("[video] MPCVR 渲染后端已启用（hdr passthrough + MPCVR 色彩管线）");
                                    Box::new(r)
                                }
                                Err(e) => {
                                    log::warn!("[video] MPCVR 启用失败，回退内置渲染器: {e}");
                                    Box::new(D3d11Renderer::new(hwnd)?)
                                }
                            }
                        } else {
                            Box::new(D3d11Renderer::new(hwnd)?)
                        };
                        (renderer, Box::new(WasapiRender::new()) as Box<dyn AudioOut>)
                    }
                };
                let mut eng = PlayerEngine::new(lib2.clone(), path, r, a)?;
                // 启动参数应用（seek/rate；失败不阻断播放）
                if rate != 1.0 {
                    eng.set_rate(rate);
                }
                if seek > 0.0 {
                    eng.seek(seek);
                }
                Ok(eng)
            }) {
                Ok(w) => w,
                Err(e) => {
                    log::error!("[video] 播放器窗口创建失败: {e}");
                    return;
                }
            };
            // 登记 HWND（close 命令目标；videorec hwnd() 直返 HWND）
            {
                let hwnd = win.hwnd();
                log::info!("[video] 播放器窗口已创建 hwnd=0x{:X}", hwnd.0 as isize);
                *PLAYER_HWND.lock().unwrap() = Some(hwnd.0 as isize);
            }
            if let Err(e) = win.run() {
                log::warn!("[video] 播放器退出: {e}");
            }
            log::info!("[video] 播放器消息循环结束");
            *PLAYER_HWND.lock().unwrap() = None;
        })
        .map_err(|e| format!("播放器线程创建失败: {e}"))?;
    Ok(())
}

/// 关闭播放器（PostMessage WM_CLOSE；未开忽略）
pub fn close_player() {
    let hwnd = PLAYER_HWND.lock().unwrap().take();
    if let Some(h) = hwnd {
        let hwnd = windows::Win32::Foundation::HWND(h as *mut core::ffi::c_void);
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    }
}

// ---------- V18 画质增强（FX 参数跨线程控制） ----------

/// PostMessage 封装（WM_FX_PARAMS = WM_APP + 10，与 videorec 窗口类一致；
/// 播放器未开 = Err）
fn post_to_player(wparam: usize, lparam: isize) -> Result<(), String> {
    let hwnd = PLAYER_HWND.lock().unwrap().clone();
    let Some(h) = hwnd else {
        return Err("播放器未运行".into());
    };
    let hwnd = windows::Win32::Foundation::HWND(h as *mut core::ffi::c_void);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_APP + 10,
            windows::Win32::Foundation::WPARAM(wparam),
            windows::Win32::Foundation::LPARAM(lparam),
        );
    }
    Ok(())
}

/// FX 参数精确设置（8 槽位）
pub fn send_fx_params(vals: [f32; 8]) -> Result<(), String> {
    post_to_player(1, Box::into_raw(Box::new(vals)) as isize)
}

/// FX 主参数档位调整（slot 槽位；dir = ±1）
pub fn send_fx_adjust(slot: usize, dir: i32) -> Result<(), String> {
    let lp = ((dir as i16) as u16 as usize) << 16 | (slot & 0xFFFF);
    post_to_player(2, lp as isize)
}

/// FX 参数复位
pub fn send_fx_reset() -> Result<(), String> {
    post_to_player(3, 0)
}

/// 选择 FX（None = OFF）
pub fn send_fx_select(name: Option<&str>) -> Result<(), String> {
    // UTF-16 NUL 结尾；Box<Vec<u16>> 传递（窗口线程消费后释放）
    let mut wide: Vec<u16> = name.map(|n| n.encode_utf16().collect()).unwrap_or_default();
    wide.push(0);
    post_to_player(4, Box::into_raw(Box::new(wide)) as isize)
}
