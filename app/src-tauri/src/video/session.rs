//! 视频录制会话：单线程循环（DDA 抓帧 → BGRA staging 读回 → nvenc 编码 → MKV 增量写盘）
//!
//! 与 JXL 动图三线程架构的本质差异（方案 4.1）：nvenc 硬编是 GPU 实时
//!（单帧 ~2ms），无需环形缓冲/回拷线程/延迟编码——一个循环全串行，简单
//! 可靠。音频由 RecorderSession 内部管理（WASAPI 采集线程 + 混音）。
//!
//! 长录制：`RecorderSession` 是 `Write + Seek` 泛型——直接传 `File`，
//! EBML 流式增量写盘（Cues 在 stop 时回填），崩溃残文件 MkvReader 可开。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tauri::Emitter;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ_WRITE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

use videorec::codec::Codec;
use videorec::recorder::{AudioMode, RecorderConfig, RecorderSession};

use crate::capture::dxgi_duplication::DesktopCapturer;
use crate::capture::monitor::MonitorInfo;

/// 会话启动参数（commands.rs 组装）
pub struct VideoSpawnParams {
    pub output_path: PathBuf,
    /// 录制目标显示器（全屏模式 = 整屏；追随模式 = 裁剪钳制边界）
    pub monitor: MonitorInfo,
    /// 追随鼠标模式：Some((fw, fh)) 裁剪窗口尺寸（已钳制偶数 + 显示器内）
    pub follow: Option<(u32, u32)>,
    pub codec: Codec,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub gop: u32,
    pub audio: AudioMode,
    pub hw: String, // "auto" | "nvenc" | "amf" | "qsv" | "cpu"
    /// HDR10 直录（scRGB → P010 → hevc main10 + BT.2020/PQ VUI）。
    /// true 但显示器非 HDR → 自动回落 SDR 路径。
    pub hdr: bool,
    /// 自动停止：媒体时长上限（秒；0 = 无限。暂停期不计）
    pub max_seconds: u32,
    /// 自动停止：文件大小上限（字节；0 = 无限）
    pub max_size_bytes: u64,
    /// 分卷阈值（字节；0 = 不分卷。单文件超阈值自动收尾 + 新文件 -partN 后缀）
    pub split_size_bytes: u64,
    /// 静默自动停止：持续无声秒数触发（0 = 关闭；音频关闭时无效）
    pub silence_stop_seconds: u32,
    /// 静默触发后的倒计时秒数（声音恢复自动取消）
    pub silence_countdown_seconds: u32,
    /// 鼠标光标叠加（DDA 帧不含指针——教程录制需软件叠加）
    pub mouse_cursor: bool,
    /// 点击效果叠加（左=青圈 / 右=红圈，0.5s 扩散）
    pub mouse_click: bool,
    /// 高亮效果叠加（点击点为中心半透明色块 1s 渐隐；None = 关）
    pub mouse_highlight: Option<([u8; 3], i32)>,
    /// 水印（文字+图片；文本与图片全空 = None 不建）
    pub watermark: Option<super::watermark::WatermarkParams>,
    /// 摄像头画中画（device 空 = None；打开失败降级无叠加继续录）
    pub camera: Option<super::camera::CameraParams>,
    pub app: tauri::AppHandle,
}

/// 运行统计快照（video://stats 事件负载 + record_status 查询）
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct VideoStats {
    pub elapsed_ms: u64,
    pub encoded_frames: u64,
    pub dropped_frames: u64,
    pub size_bytes: u64,
    pub encoder: String, // 实际编码器名（探测链结果，告知前端）
    /// 暂停中（elapsed_ms 已扣除暂停时长 = 媒体时长）
    pub paused: bool,
    /// 瞬时帧率（最近 1s 编码帧数——统计节拍差分）
    pub fps: u32,
}

/// 停止结果
#[derive(Clone, Debug, serde::Serialize)]
pub struct VideoResult {
    pub path: String,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub codec: String,
    pub encoder: String,
}

/// 运行中的视频会话句柄（命令层持有；Drop 时若未停止仅置 stop 标志——
/// 正常路径必须显式 finish，退出保护 flush_on_exit 负责兜底）
pub struct VideoSession {
    pub stop: Arc<AtomicBool>,
    /// 暂停标志（命令层置位；录制线程消费：转换沿调用 RecorderSession 暂停语义）
    pub pause: Arc<AtomicBool>,
    pub handle: Option<std::thread::JoinHandle<()>>,
    pub stats: Arc<Mutex<VideoStats>>,
    pub result: Arc<Mutex<Option<VideoResult>>>,
    pub output_path: PathBuf,
    /// 全部分卷路径（含首文件；录制线程 push——cancel 时全部删除）
    pub parts: Arc<Mutex<Vec<PathBuf>>>,
    /// 探测出的实际编码器（启动即确定；stats.encoder 的稳定副本）
    pub encoder: String,
    /// AppHandle 副本（完成动作执行用——收尾路径多处无 app 上下文）
    pub app: tauri::AppHandle,
    /// 沉浸式（热键 OSD）模式的 AppHandle：Some = 结束时需 teardown（注销
    /// 停止热键 + 停置顶看护 + 还原面板）；None = 面板路径无沉浸式布置
    pub osd_app: Option<tauri::AppHandle>,
}

/// 编码器名（Codec → 容器轨名/结果字符串）
fn codec_name(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
    }
}

/// 按硬编配置解析编码器名探测链（auto = nvenc → amf → qsv → libopenh264）
fn encoder_candidates(hw: &str, codec: Codec) -> Vec<String> {
    let (nv, amf, qsv, sw) = match codec {
        Codec::H264 => (
            "h264_nvenc",
            "h264_amf",
            "h264_qsv",
            "libopenh264",
        ),
        Codec::Hevc => ("hevc_nvenc", "hevc_amf", "hevc_qsv", "libopenh264"),
    };
    match hw {
        "nvenc" => vec![nv.to_string(), sw.to_string()],
        "amf" => vec![amf.to_string(), sw.to_string()],
        "qsv" => vec![qsv.to_string(), sw.to_string()],
        "cpu" => vec![sw.to_string()],
        _ => vec![nv.to_string(), amf.to_string(), qsv.to_string(), sw.to_string()],
    }
}

/// VideoEncoder 的跨线程移交包装。
///
/// FFmpeg 上下文含裸指针默认非 Send；本模块语义 = 调用线程 open（探测链
/// 前置失败即返回）后**移交**录制线程独占使用，移交后无任何并发访问——
/// 单线程独占语义下 Send 安全（对齐 windows-rs COM 包装的同款论证）。
struct SendEncoder(videorec::ffmpeg::encoder::VideoEncoder);
unsafe impl Send for SendEncoder {}
impl SendEncoder {
    /// 解包（方法调用捕获整个 SendEncoder 变量——模式解构 `let SendEncoder(e) = enc`
    /// 在 Rust 2021 精确捕获下仍只捕非 Send 的 `enc.0` 字段，等于没包装）
    fn into_inner(self) -> videorec::ffmpeg::encoder::VideoEncoder {
        self.0
    }
}

/// 启动视频录制会话（单线程：抓帧→编码→写盘全串行）
///
/// 失败语义：任何前置步骤（FFmpeg probe / 编码器打开 / DDA 创建 / MKV 头）
/// 失败立即返回 Err——不建线程不留半启动（对齐 videorec audio 的无半启动契约）。
pub fn spawn_session(p: VideoSpawnParams) -> Result<VideoSession, String> {
    log::info!(
        "[video] spawn_session 阈值: max_seconds={} max_size_bytes={} split_size_bytes={} bitrate_bps={} fps={}",
        p.max_seconds,
        p.max_size_bytes,
        p.split_size_bytes,
        p.bitrate_bps,
        p.fps
    );
    // 1. FFmpeg DLL 前置探测（缺 DLL 明确报错）
    let lib = Arc::new(videorec::ffmpeg::FFmpegLib::probe().map_err(|e| {
        format!("视频组件不可用：{e}（需在 exe 目录 ffmpeg\\bin 放置 FFmpeg DLL）")
    })?);

    // 2. DDA 采集器（目标显示器；HDR 直录：prefer_hdr=true → scRGB f16 帧；
    // SDR 路径：false → 系统自动 HDR→SDR，帧恒 BGRA8）
    let monitor = p.monitor.clone();
    let follow = p.follow;
    let hdr = p.hdr && monitor.is_hdr();
    if p.hdr && !hdr {
        log::warn!("[video] 请求 HDR 直录但显示器非 HDR 模式，回落 SDR 路径");
    }
    let mut capturer =
        DesktopCapturer::for_monitor(&monitor, hdr).map_err(|e| format!("DDA 采集创建失败: {e}"))?;
    let (w, h) = (monitor.width, monitor.height);

    // 3. 编码器探测链（find_encoder_by_name 逐个试开；HDR = hevc main10 P010
    //    + BT.2020/PQ VUI，探测链强制 hevc；SDR = 用户 codec）。
    //    编码尺寸：全屏 = 显示器；追随 = 裁剪窗（GPU/staging 仍按显示器建）
    let fps = p.fps.clamp(10, 144);
    let (ew, eh) = follow.unwrap_or((w, h));
    let enc_codec = if hdr { Codec::Hevc } else { p.codec };
    let mut enc = None;
    let mut used = String::new();
    let mut last_err = String::new();
    for name in encoder_candidates(&p.hw, enc_codec) {
        let r = if hdr {
            videorec::ffmpeg::encoder::VideoEncoder::open_p010(
                lib.clone(),
                &name,
                ew,
                eh,
                fps,
                p.bitrate_bps,
                p.gop,
                &[("preset", "p4")],
            )
        } else {
            videorec::ffmpeg::encoder::VideoEncoder::open(
                lib.clone(),
                &name,
                ew,
                eh,
                fps,
                p.bitrate_bps,
                p.gop,
                &[("preset", "p4")],
            )
        };
        match r {
            Ok(e) => {
                used = name.to_string();
                enc = Some(e);
                break;
            }
            Err(e) => last_err = format!("{name}: {e}"),
        }
    }
    let mut enc = enc.ok_or_else(|| format!("无可用视频编码器（{last_err}）"))?;
    log::info!(
        "[video] 编码器就绪: {used}（{ew}x{eh} @{fps}fps{}{}）",
        if hdr { " HDR10" } else { "" },
        if follow.is_some() { " 追随鼠标" } else { "" }
    );

    // 4. MKV 会话（File sink = 长录制增量写盘；音频设备在 start 内打开）
    let file = std::fs::File::create(&p.output_path)
        .map_err(|e| format!("创建输出文件失败 {}: {e}", p.output_path.display()))?;
    let mut session = RecorderSession::start(
        file,
        RecorderConfig {
            codec: enc_codec,
            width: ew,
            height: eh,
            audio: p.audio,
        },
    )
    .map_err(|e| format!("MKV 会话启动失败: {e}"))?;
    for warn in session.warnings() {
        log::warn!("[video] 音频降级: {warn}");
    }

    // 5. 帧路径资源（建在 DDA 设备上）：
    // - SDR：BGRA staging 纹理（直接读回 push_bgra）
    // - HDR：P010Gpu 转换器（scRGB→PQ 域 YCbCr→P010 双平面→push_p010）
    let device = capturer.device().clone();
    let ctx: ID3D11DeviceContext = capturer.context().clone();
    enum FramePath {
        Sdr {
            staging: ID3D11Texture2D,
        },
        Hdr {
            gpu: super::p010_gpu::P010Gpu,
        },
    }
    let path = if hdr {
        let gpu = super::p010_gpu::P010Gpu::new(&device, ctx.clone(), w, h)
            .map_err(|e| format!("P010 GPU 转换器创建失败: {e}"))?;
        FramePath::Hdr { gpu }
    } else {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            // READ|WRITE：读回 + 鼠标光标/点击效果 CPU 叠加原地写
            CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .map_err(|e| format!("staging 纹理创建失败: {e}"))?;
        }
        FramePath::Sdr {
            staging: t.ok_or("staging None")?,
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new(VideoStats {
        encoder: used.clone(),
        ..Default::default()
    }));
    let result: Arc<Mutex<Option<VideoResult>>> = Arc::new(Mutex::new(None));
    let parts: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(vec![p.output_path.clone()]));
    let encoder_used = used.clone();
    let output_path = p.output_path.clone();

    let stop2 = Arc::clone(&stop);
    let pause2 = Arc::clone(&pause);
    let stats2 = Arc::clone(&stats);
    let result2 = Arc::clone(&result);
    let parts2 = Arc::clone(&parts);
    let app = p.app.clone();
    let result_codec = enc_codec; // 结果 codec（HDR 强制 hevc）
    let p_max_seconds = p.max_seconds;
    let p_max_size_bytes = p.max_size_bytes;
    let p_split_size_bytes = p.split_size_bytes;
    let p_silence_stop = p.silence_stop_seconds;
    let p_silence_countdown = p.silence_countdown_seconds.max(1);
    let p_audio_on = !matches!(p.audio, AudioMode::Off);
    let p_mouse_cursor = p.mouse_cursor;
    let p_mouse_click = p.mouse_click;
    let p_mouse_highlight = p.mouse_highlight;
    let p_watermark = p.watermark;
    let p_camera = p.camera;
    // 分卷换轨参数（重开编码器 + 新会话；AudioMode Clone）
    let split_lib = Arc::clone(&lib);
    let split_enc_name = used.clone();
    let split_hdr = hdr;
    let split_w = ew;
    let split_h = eh;
    let split_fps = fps;
    let split_bitrate = p.bitrate_bps;
    let split_gop = p.gop;
    let split_audio = p.audio.clone();
    // 追随鼠标裁剪的显示器原点（虚拟桌面坐标；光标 → 显示器相对坐标换算）
    let monitor_left = monitor.left;
    let monitor_top = monitor.top;
    let enc = SendEncoder(enc);
    let handle = std::thread::Builder::new()
        .name("video-record".into())
        .spawn(move || {
            // 解包：此后编码器仅本线程独占使用（Send 移交完成）
            let mut enc = enc.into_inner();
            // 鼠标叠加器（光标位图缓存 + 点击钩子；Drop 自动停钩子线程）
            let mut overlay = super::cursor_overlay::CursorOverlay::new(
                p_mouse_cursor,
                p_mouse_click,
                p_mouse_highlight,
            );
            overlay.start();
            // 水印（文字 GDI + 图片 WIC；图片启用时内部初始化 COM，Drop 释放）
            let mut wm = p_watermark.map(super::watermark::Watermark::new);
            // 摄像头画中画（MF 采集线程；Drop 停线程。打开失败 None 降级）
            let mut cam = p_camera.and_then(super::camera::Camera::new);
            let t0 = Instant::now();
            let mut stats_inner = VideoStats {
                encoder: encoder_used.clone(),
                ..Default::default()
            };
            let min_interval = std::time::Duration::from_millis(1000 / fps as u64);
            let mut last_frame = Instant::now() - min_interval; // 首帧立即收
            let mut last_stats_emit = Instant::now();
            let mut session_ms: u64 = 0;
            let mut first_frame = true;
            // 暂停状态机：paused_now 跟随 pause 标志（转换沿调用会话暂停语义）；
            // paused_total 累计暂停时长（elapsed 展示媒体时长 = 墙钟 − 暂停）
            let mut paused_now = false;
            let mut paused_total_ms: u64 = 0;
            let mut pause_started: Option<Instant> = None;
            // 自动停止原因（时长/大小阈值命中；None = 未触发）
            let mut auto_stop: Option<&'static str> = None;
            let max_ms = (p_max_seconds as u64).saturating_mul(1000);
            // 分卷状态：part_no 当前卷号 / part_t0 当前卷时基（新卷时间戳归零）
            // / cur_path 当前卷路径（stats 体积 + 收尾结果均按当前卷）
            let mut part_no: u32 = 1;
            let mut part_t0 = t0;
            let mut cur_path = output_path.clone();
            // 静默倒计时：Some = 倒计时中（声音恢复取消；到时自动停）
            let mut silence_countdown_end: Option<Instant> = None;
            // 上次统计节拍的累计编码帧数（FPS 差分基线）
            let mut last_emitted_frames: u64 = 0;

            'outer: loop {
                // 统计事件（1s；elapsed = 媒体时长，暂停不计）
                if last_stats_emit.elapsed() >= std::time::Duration::from_secs(1) {
                    let mut media_ms = t0.elapsed().as_millis() as u64;
                    if let Some(t) = pause_started {
                        media_ms = media_ms.saturating_sub(t.elapsed().as_millis() as u64);
                    }
                    stats_inner.elapsed_ms = media_ms.saturating_sub(paused_total_ms);
                    stats_inner.paused = paused_now;
                    stats_inner.size_bytes = std::fs::metadata(&cur_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    // 瞬时 FPS：本节拍编码帧数差分（暂停期不更新 = 显示冻结值）
                    if !paused_now {
                        stats_inner.fps = (stats_inner.encoded_frames - last_emitted_frames)
                            .min(999) as u32;
                        last_emitted_frames = stats_inner.encoded_frames;
                    }
                    {
                        let mut g = stats2.lock().unwrap();
                        *g = stats_inner.clone();
                    }
                    let _ = app.emit("video://stats", &stats_inner);
                    last_stats_emit = Instant::now();
                    // 自动停止判定（媒体时长 / 文件大小；暂停期 elapsed 冻结 = 暂停不计时）
                    if auto_stop.is_none() {
                        if max_ms > 0 && stats_inner.elapsed_ms >= max_ms {
                            auto_stop = Some("时长上限");
                        } else if p_max_size_bytes > 0
                            && stats_inner.size_bytes >= p_max_size_bytes
                        {
                            auto_stop = Some("大小上限");
                        }
                        if let Some(reason) = auto_stop {
                            log::info!("[video] 自动停止（{reason}）：{}", output_path.display());
                            break 'outer;
                        }
                    }
                    // 静默自动停止（音频开启 + 未暂停才判定；暂停期块被丢弃
                    // 不计静默）。倒计时中声音恢复 → 取消续录；到时 → 停止
                    if p_silence_stop > 0 && p_audio_on && !paused_now {
                        match silence_countdown_end {
                            Some(end) => {
                                let now = Instant::now();
                                if now >= end {
                                    log::info!("[video] 静默超时自动停止：{}", output_path.display());
                                    auto_stop = Some("静默超时");
                                    break 'outer;
                                }
                                if session.silence_ms() < 1000 {
                                    // 最近 1s 内有声 → 取消倒计时续录
                                    silence_countdown_end = None;
                                    log::info!("[video] 声音恢复，静默倒计时取消");
                                    let _ = app.emit("video://silence-countdown", 0u32);
                                } else {
                                    let remaining =
                                        ((end - now).as_millis() / 1000 + 1) as u32;
                                    let _ = app.emit("video://silence-countdown", remaining);
                                }
                            }
                            None => {
                                if session.silence_ms() >= p_silence_stop as u64 * 1000 {
                                    silence_countdown_end = Some(
                                        Instant::now()
                                            + std::time::Duration::from_secs(
                                                p_silence_countdown as u64,
                                            ),
                                    );
                                    log::info!(
                                        "[video] 静默 {}s，{}s 后自动停止（声音恢复取消）",
                                        p_silence_stop,
                                        p_silence_countdown
                                    );
                                    let _ = app.emit("video://silence-countdown", p_silence_countdown);
                                }
                            }
                        }
                    }
                    // 分卷判定（当前卷体积 ≥ 阈值；暂停期不判定——体积冻结，
                    // 恢复后下一节拍补判）。先建新链（编码器/文件/会话），全部
                    // 就绪才收尾旧卷——失败则旧链原样走正常收尾（录制降级为停止）
                    if p_split_size_bytes > 0
                        && !paused_now
                        && stats_inner.size_bytes >= p_split_size_bytes
                    {
                        let new_path = part_path(&output_path, part_no + 1);
                        let new_chain = open_video_encoder(
                            &split_lib,
                            &split_enc_name,
                            split_hdr,
                            split_w,
                            split_h,
                            split_fps,
                            split_bitrate,
                            split_gop,
                        )
                        .and_then(|e| {
                            std::fs::File::create(&new_path)
                                .map_err(|e| format!("创建分卷文件失败: {e}"))
                                .map(|f| (e, f))
                        })
                        .and_then(|(e, f)| {
                            RecorderSession::start(
                                f,
                                RecorderConfig {
                                    codec: result_codec,
                                    width: split_w,
                                    height: split_h,
                                    audio: split_audio.clone(),
                                },
                            )
                            .map_err(|e| format!("分卷 MKV 会话启动失败: {e}"))
                            .map(|s| (e, s))
                        });
                        match new_chain {
                            Ok((new_enc, new_sess)) => {
                                // 旧卷收尾（排空 → finalize → part 事件；不重置录制 UI）
                                if let Ok(pkts) = enc.finish() {
                                    for pkt in pkts {
                                        let _ = session.push_video_frame_at(
                                            session_ms,
                                            pkt.keyframe,
                                            &pkt.annexb,
                                        );
                                    }
                                }
                                let part_dur = session
                                    .stop()
                                    .map(|(d, _)| d)
                                    .unwrap_or(session_ms);
                                let part_size = std::fs::metadata(&cur_path)
                                    .map(|m| m.len())
                                    .unwrap_or(0);
                                let pr = VideoResult {
                                    path: cur_path.to_string_lossy().into_owned(),
                                    duration_ms: part_dur,
                                    size_bytes: part_size,
                                    codec: codec_name(result_codec).to_string(),
                                    encoder: encoder_used.clone(),
                                };
                                log::info!(
                                    "[video] 分卷 part{part_no} 完成: {}（{:.1}s / {:.1}MB）",
                                    pr.path,
                                    part_dur as f64 / 1000.0,
                                    part_size as f64 / 1048576.0
                                );
                                let _ = app.emit("video://part", &pr);
                                // 换轨：新编码器/会话 + 时间戳归零 + 首帧重置
                                enc = new_enc;
                                session = new_sess;
                                part_no += 1;
                                parts2.lock().unwrap().push(new_path.clone());
                                cur_path = new_path;
                                part_t0 = Instant::now();
                                session_ms = 0;
                                first_frame = true;
                                stats_inner.size_bytes = 0;
                            }
                            Err(e) => {
                                log::error!("[video] 分卷失败，转为停止: {e}");
                                auto_stop = Some("分卷失败");
                                break 'outer;
                            }
                        }
                    }
                }
                if stop2.load(Ordering::Acquire) {
                    break 'outer;
                }
                // 暂停转换沿（会话内 MediaClock 冻结/折叠；恢复后时间戳自动续接）
                let want_pause = pause2.load(Ordering::Acquire);
                if want_pause != paused_now {
                    if want_pause {
                        session.pause();
                        pause_started = Some(Instant::now());
                        log::info!("[video] 录制暂停");
                    } else {
                        if let Some(t) = pause_started.take() {
                            paused_total_ms += t.elapsed().as_millis() as u64;
                        }
                        session.resume();
                        // 暂停期音频块被丢弃不计静默——恢复后重置计时，
                        // 否则立即误触发静默停止；同时取消进行中的倒计时
                        session.reset_silence();
                        if silence_countdown_end.take().is_some() {
                            let _ = app.emit("video://silence-countdown", 0u32);
                        }
                        log::info!("[video] 录制恢复");
                    }
                    paused_now = want_pause;
                    // 立即广播状态（不等 1s 节拍；前端按钮即时反馈）
                    stats_inner.paused = paused_now;
                    {
                        let mut g = stats2.lock().unwrap();
                        *g = stats_inner.clone();
                    }
                    let _ = app.emit("video://stats", &stats_inner);
                }
                if paused_now {
                    // 暂停期：帧不编码（省 GPU/CPU），但 DDA 队列必须持续排空
                    // （不 release = 3 帧后 acquire 阻塞超时）；音频泵入继续
                    // （MediaClock 暂停区间内到达块自动丢弃）
                    match capturer.acquire_frame_timeout(50) {
                        Ok((_info, _tex, _fmt)) => {
                            capturer.release_frame();
                        }
                        Err(_) => {}
                    }
                    let _ = session.pump_audio();
                    continue;
                }

                // 抓帧（50ms 短超时对齐动图；WAIT_TIMEOUT = 画面静止心跳继续）
                let frame = capturer.acquire_frame_timeout(50);
                match frame {
                    Ok((info, tex, _fmt)) => {
                        // DDA 纯鼠标帧丢弃（LastPresentTime==0 且非首帧，动图同语义）
                        if info.LastPresentTime == 0 && !first_frame {
                            capturer.release_frame();
                            continue;
                        }
                        // fps 限流
                        if last_frame.elapsed() < min_interval {
                            capturer.release_frame();
                            continue;
                        }
                        // 时间戳：session 毫秒（当前卷时基；首视频帧 = 0；
                        // 暂停后恢复经 MediaClock 折叠；分卷后归零续录）
                        let now_ms = part_t0.elapsed().as_millis() as u64;
                        session_ms = now_ms;
                        // 追随鼠标：裁剪窗居中光标，钳制显示器内（偶数对齐——
                        // P010 UV 2×2 块完整；光标离屏 = 贴边跟随）
                        let (crop_x, crop_y) = match follow {
                            Some((fw, fh)) => {
                                let mut pt = windows::Win32::Foundation::POINT::default();
                                let _ = unsafe {
                                    windows::Win32::UI::WindowsAndMessaging::GetCursorPos(
                                        &mut pt,
                                    )
                                };
                                let mx = pt.x - monitor_left;
                                let my = pt.y - monitor_top;
                                let cx = (mx - fw as i32 / 2)
                                    .clamp(0, w as i32 - fw as i32)
                                    & !1;
                                let cy = (my - fh as i32 / 2)
                                    .clamp(0, h as i32 - fh as i32)
                                    & !1;
                                (cx as usize, cy as usize)
                            }
                            None => (0usize, 0usize),
                        };
                        // 帧读回 + 编码（双路径：SDR 直读 BGRA / HDR GPU 转 P010）
                        let push: Result<u64, String> = match &path {
                            FramePath::Sdr { staging } => {
                                // GPU→staging→CPU 读回 BGRA（READ_WRITE：鼠标叠加原地写）
                                let read = unsafe {
                                    ctx.CopyResource(staging, &tex);
                                    capturer.release_frame();
                                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                                    ctx.Map(
                                        staging,
                                        0,
                                        D3D11_MAP_READ_WRITE,
                                        0,
                                        Some(&mut mapped),
                                    )
                                    .map_err(|e| format!("staging Map: {e}"))
                                    .map(|_| mapped)
                                };
                                let Ok(mapped) = read else {
                                    let e = read.err().unwrap();
                                    log::warn!("[video] {e}");
                                    stats_inner.dropped_frames += 1;
                                    continue;
                                };
                                // 安全性：mapped 指针在 Unmap 前有效；切片仅在本块内使用
                                unsafe {
                                    let data = std::slice::from_raw_parts_mut(
                                        mapped.pData as *mut u8,
                                        mapped.RowPitch as usize * h as usize,
                                    );
                                    let pitch = mapped.RowPitch as usize;
                                    // 追随鼠标：子区域切片（显式末界 = 末行
                                    // (fh−1)·pitch + fw·4；stride 仍为整图行距）
                                    let r = match follow {
                                        Some((fw, fh)) => {
                                            let off = crop_y * pitch + crop_x * 4;
                                            let end = off
                                                + (fh as usize - 1) * pitch
                                                + fw as usize * 4;
                                            let cropped = &mut data[off..end];
                                            overlay.blend_bgra_off(
                                                cropped,
                                                pitch,
                                                fw as i32,
                                                fh as i32,
                                                crop_x as i32,
                                                crop_y as i32,
                                            );
                                            if let Some(wm) = wm.as_mut() {
                                                // 水印锚定输出帧（裁剪窗）边角
                                                wm.blend_bgra(cropped, pitch, fw as i32, fh as i32);
                                            }
                                            if let Some(cam) = cam.as_mut() {
                                                cam.blend_bgra(
                                                    cropped, pitch, fw as i32, fh as i32,
                                                );
                                            }
                                            enc.push_bgra(cropped, pitch, false)
                                        }
                                        None => {
                                            // 鼠标光标/点击效果叠加（sRGB 域原地混合）
                                            overlay.blend_bgra(
                                                data,
                                                pitch,
                                                w as i32,
                                                h as i32,
                                            );
                                            if let Some(wm) = wm.as_mut() {
                                                wm.blend_bgra(data, pitch, w as i32, h as i32);
                                            }
                                            if let Some(cam) = cam.as_mut() {
                                                cam.blend_bgra(data, pitch, w as i32, h as i32);
                                            }
                                            enc.push_bgra(data, pitch, false)
                                        }
                                    };
                                    ctx.Unmap(staging, 0);
                                    r
                                }
                            }
                            FramePath::Hdr { gpu } => {
                                // GPU：scRGB→PQ 域 YCbCr→P010 双平面→staging
                                if let Err(e) = gpu.process_frame(&tex) {
                                    log::warn!("[video] P010 转换失败（丢帧）: {e}");
                                    capturer.release_frame();
                                    stats_inner.dropped_frames += 1;
                                    continue;
                                }
                                capturer.release_frame();
                                let uv_h = h.div_ceil(2) as usize;
                                match gpu.map_yuv() {
                                    Ok((my, muv)) => unsafe {
                                        let y = std::slice::from_raw_parts_mut(
                                            my.pData as *mut u8,
                                            my.RowPitch as usize * h as usize,
                                        );
                                        let uv = std::slice::from_raw_parts_mut(
                                            muv.pData as *mut u8,
                                            muv.RowPitch as usize * uv_h,
                                        );
                                        let (sy, suv) =
                                            (my.RowPitch as usize, muv.RowPitch as usize);
                                        // 追随鼠标：双平面子区域切片（Y 样点 2B /
                                        // UV 2×2 块 4B；crop 偶数对齐保证块完整）
                                        let r = match follow {
                                            Some((fw, fh)) => {
                                                let off_y = crop_y * sy + crop_x * 2;
                                                let end_y = off_y
                                                    + (fh as usize - 1) * sy
                                                    + fw as usize * 2;
                                                let off_uv =
                                                    (crop_y / 2) * suv + (crop_x / 2) * 4;
                                                let end_uv = off_uv
                                                    + (fh as usize / 2 - 1) * suv
                                                    + fw as usize * 2;
                                                let cy_ = &mut y[off_y..end_y];
                                                let cuv = &mut uv[off_uv..end_uv];
                                                overlay.blend_p010_off(
                                                    cy_,
                                                    cuv,
                                                    sy,
                                                    suv,
                                                    fw as i32,
                                                    fh as i32,
                                                    crop_x as i32,
                                                    crop_y as i32,
                                                );
                                                if let Some(wm) = wm.as_mut() {
                                                    // 水印锚定输出帧（裁剪窗）边角
                                                    wm.blend_p010(
                                                        cy_, cuv, sy, suv, fw as i32, fh as i32,
                                                    );
                                                }
                                                if let Some(cam) = cam.as_mut() {
                                                    cam.blend_p010(
                                                        cy_, cuv, sy, suv, fw as i32, fh as i32,
                                                    );
                                                }
                                                enc.push_p010(cy_, cuv, sy, suv, false)
                                            }
                                            None => {
                                                // 鼠标光标/点击效果叠加（PQ 域原地
                                                // 混合，数学与 shader 对偶；UV 下采样）
                                                overlay.blend_p010(
                                                    y,
                                                    uv,
                                                    sy,
                                                    suv,
                                                    w as i32,
                                                    h as i32,
                                                );
                                                if let Some(wm) = wm.as_mut() {
                                                    wm.blend_p010(
                                                        y, uv, sy, suv, w as i32, h as i32,
                                                    );
                                                }
                                                if let Some(cam) = cam.as_mut() {
                                                    cam.blend_p010(
                                                        y, uv, sy, suv, w as i32, h as i32,
                                                    );
                                                }
                                                enc.push_p010(y, uv, sy, suv, false)
                                            }
                                        };
                                        gpu.unmap();
                                        r
                                    },
                                    Err(e) => {
                                        log::warn!("[video] P010 读回失败（丢帧）: {e}");
                                        stats_inner.dropped_frames += 1;
                                        continue;
                                    }
                                }
                            }
                        };
                        match push {
                            Ok(_) => {
                                stats_inner.encoded_frames += 1;
                                first_frame = false;
                                last_frame = Instant::now();
                            }
                            Err(e) => {
                                log::warn!("[video] 编码失败（丢帧）: {e}");
                                stats_inner.dropped_frames += 1;
                            }
                        }
                        // 编码包即时落盘（nvenc 延迟 ≤3 帧；同批包共用当前墙钟——
                        // 帧历史墙钟方案与 muxer 簇基准冲突：音频块走设备 pts
                        // 实时钟，会先于滞后 nvenc 延迟的视频帧墙钟推进簇基准
                        // → timecode 回退。共戳仅发生在起编/收尾批，实测播放正常）
                        if let Err(e) = flush_packets(&mut enc, &mut session, session_ms) {
                            log::error!("[video] 写盘失败: {e}");
                            break 'outer;
                        }
                        // 音频泵入（RecorderSession 内部采集线程 → MKV 音轨）
                        let _ = session.pump_audio();
                    }
                    Err(e) => {
                        let code = e.code();
                        if code == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT {
                            // 静止期也要泵音频（ RecorderSession 内部时钟推进，
                            // 不泵 = 音轨长时间缺块；50ms 一次开销可忽略）
                            let _ = session.pump_audio();
                            continue;
                        }
                        if code == windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST {
                            log::warn!("[video] DDA ACCESS_LOST，重建采集");
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            if capturer.recreate_duplication().is_err() {
                                stats_inner.dropped_frames += 1;
                            }
                            continue;
                        }
                        log::error!("[video] 采集失败: {e}");
                        break 'outer;
                    }
                }
            }

            // 收尾：暂停态先恢复（MediaClock 计价完整；末包时间戳不落入暂停区间）
            if paused_now {
                if let Some(t) = pause_started.take() {
                    paused_total_ms += t.elapsed().as_millis() as u64;
                }
                session.resume();
            }
            // 收尾：排空编码器 → 最后包落盘 → finalize MKV（Cues 回填）
            let final_ms = session_ms;
            match enc.finish() {
                Ok(packets) => {
                    for pkt in packets {
                        let _ = session.push_video_frame_at(final_ms, pkt.keyframe, &pkt.annexb);
                    }
                }
                Err(e) => log::warn!("[video] 编码器排空失败: {e}"),
            }
            let duration_ms = match session.stop() {
                Ok((d, _file)) => d,
                Err(e) => {
                    log::error!("[video] MKV finalize: {e}");
                    final_ms
                }
            };
            let size = std::fs::metadata(&cur_path).map(|m| m.len()).unwrap_or(0);
            let r = VideoResult {
                path: cur_path.to_string_lossy().into_owned(),
                duration_ms,
                size_bytes: size,
                codec: codec_name(result_codec).to_string(),
                encoder: encoder_used.clone(),
            };
            log::info!(
                "[video] 录制完成: {}（{:.1}s / {} 帧 / {:.1}MB{}）",
                r.path,
                duration_ms as f64 / 1000.0,
                stats_inner.encoded_frames,
                size as f64 / 1048576.0,
                if part_no > 1 {
                    format!("，共 {part_no} 卷（帧数为累计）")
                } else {
                    String::new()
                }
            );
            *result2.lock().unwrap() = Some(r.clone());
            stats_inner.elapsed_ms = (t0.elapsed().as_millis() as u64).saturating_sub(paused_total_ms);
            stats_inner.paused = false;
            stats_inner.size_bytes = size;
            {
                let mut g = stats2.lock().unwrap();
                *g = stats_inner.clone();
            }
            let _ = app.emit("video://done", &r);
            // 自动停止路径的槽位/沉浸式收尾（finalize + done 已完成）。
            // 手动停止路径（stop/cancel/flush）由 finish_session 负责——此处
            // take 为 None 即对方已接管，幂等跳过（双收尾安全竞态）
            if auto_stop.is_some() {
                super::commands::auto_stopped_cleanup(&app);
            }
        })
        .map_err(|e| format!("录制线程创建失败: {e}"))?;

    Ok(VideoSession {
        stop,
        pause,
        handle: Some(handle),
        stats,
        result,
        output_path: p.output_path,
        parts,
        encoder: used,
        app: p.app,
        osd_app: None, // 由 commands 层按模式回填（热键 OSD 路径 Some）
    })
}

/// 分卷路径：`video_...-partN.mkv`（N≥2；首文件原名）
fn part_path(base: &std::path::Path, no: u32) -> PathBuf {
    if no <= 1 {
        return base.to_path_buf();
    }
    let stem = base
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut p = base.with_file_name(format!("{stem}-part{no}"));
    if let Some(ext) = base.extension() {
        p.set_extension(ext);
    }
    p
}

/// 按已知可用名重开编码器（分卷换轨；探测链在启动时已定型）
fn open_video_encoder(
    lib: &Arc<videorec::ffmpeg::FFmpegLib>,
    name: &str,
    hdr: bool,
    w: u32,
    h: u32,
    fps: u32,
    bitrate_bps: u64,
    gop: u32,
) -> Result<videorec::ffmpeg::encoder::VideoEncoder, String> {
    if hdr {
        videorec::ffmpeg::encoder::VideoEncoder::open_p010(
            lib.clone(),
            name,
            w,
            h,
            fps,
            bitrate_bps,
            gop,
            &[("preset", "p4")],
        )
    } else {
        videorec::ffmpeg::encoder::VideoEncoder::open(
            lib.clone(),
            name,
            w,
            h,
            fps,
            bitrate_bps,
            gop,
            &[("preset", "p4")],
        )
    }
    .map_err(|e| format!("编码器重开失败 {name}: {e}"))
}

/// 编码包落盘（take_packets → push_video_frame_at；同批包共用当前墙钟）
fn flush_packets(
    enc: &mut videorec::ffmpeg::encoder::VideoEncoder,
    session: &mut RecorderSession<std::fs::File>,
    ms: u64,
) -> Result<(), String> {
    for pkt in enc.take_packets() {
        session
            .push_video_frame_at(ms, pkt.keyframe, &pkt.annexb)
            .map_err(|e| format!("MKV 写帧: {e}"))?;
    }
    Ok(())
}

/// 统计累计计数（video_record_status 用）
static VIDEO_TOTAL_EMITTED: AtomicU64 = AtomicU64::new(0);
#[allow(dead_code)]
pub fn bump_total() {
    VIDEO_TOTAL_EMITTED.fetch_add(1, Ordering::Relaxed);
}
