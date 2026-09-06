//! 录制会话核心：三线程流水线（v3 设计）
//!
//! ```text
//! 线程A 抓帧     DDA acquire(50ms) → GPU 裁剪+转换+哈希 → staging 池 → 票据
//!               （CPU < 1ms/帧；不等编码、不等回拷、不等磁盘）
//! 线程B 回拷入环 延迟 Map → 哈希去重（重复帧跳过大回拷）
//!               → 水位自适应 zstd → 时间环形缓冲（30s + 字节保险丝）
//! 线程C 编码     BELOW_NORMAL 优先级，PQ16 直喂 libjxl（零色彩转换）
//!               增量写盘；机会式消化（静止期=编码空档）
//! ```
//!
//! 帧时长语义（时间轴恒定正确）：
//! - 帧 i (i≥1) 的 duration = pts[i+1] - pts[i]；末帧 = end_pts - pts[n]
//! - 首帧基准为 0（动画从 t=0 显示首帧，覆盖录制起始前的静止画面）
//! - 去重/静止期的时长自然并入下一帧，播放速度恒等于真实速度
//!
//! 停止序列（无死锁）：stop 标志 → 线程A 退出并记录 end_pts → 关闭票据队列
//! → 线程B drain 后关闭环形 → 线程C drain 后 finalize → 写入 result。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tauri::Emitter;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO;

use crate::capture::dxgi_duplication::DesktopCapturer;
use crate::capture::monitor::enumerate_monitors;
use crate::capture::wgc::{WgcCapturer, WgcFrameInfo};
use crate::color::PixelFormat;
use crate::encode::jxl::{animation_effort, AnimationJxlEncoder};
use crate::encode::QualityLevel;

use super::gpu::{RecordGpu, STAGING_POOL_DEPTH};

/// 绝对上限：30 秒（环形缓冲内存预算依据；config.recording.max_seconds
/// 钳制 1..=30 后换算的运行时上限 record_max_ms 不会超过它）
pub const MAX_RECORD_MS: u64 = 30_000;

/// DDA ACCESS_LOST（MPO 平面重排/独占全屏切换/桌面合成器重置）后
/// duplication 重建重试参数：最多 3 次，每次间隔 500ms，仍失败才致命退出
/// （WGC item Closed 后 recreate 重建同语义，共用此参数）
const ACCESS_LOST_REBUILD_MAX: u32 = 3;
const ACCESS_LOST_RETRY_INTERVAL_MS: u64 = 500;

/// 录制采集后端选择（SpawnParams 携带；commands 按 config×osd 解析后传入）
///
/// - `Wgc`：强制 WGC（Windows.Graphics.Capture）——失败即报错，不兜底
/// - `Dda`：强制 DDA（DXGI Desktop Duplication，原路径）
/// - `Auto`：WGC 优先 + DDA 兜底（游戏模式；spawn_session 内 for_monitor
///   失败时 log warn "WGC 不可用" 回退 DDA）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureBackend {
    Auto,
    Wgc,
    Dda,
}

/// 帧元信息（DDA/WGC 双后端统一包装）
enum FrameKind {
    /// DDA 帧（LastPresentTime==0 且非首帧 = 纯鼠标帧，丢弃）
    Dda(DXGI_OUTDUPL_FRAME_INFO),
    /// WGC 帧（无纯鼠标帧概念——cursor 已禁，帧全接受）
    Wgc(WgcFrameInfo),
}

/// 双后端采集器（私有统一门面；错误码约定已对齐：
/// WAIT_TIMEOUT = 静止心跳 / ACCESS_LOST = 失效需 recreate）
enum AnyCapturer {
    Dda(DesktopCapturer),
    Wgc(WgcCapturer),
}

impl AnyCapturer {
    /// 取一帧（DDA：AcquireNextFrame；WGC：事件驱动 TryGetNextFrame；
    /// 超时语义一致——桌面静止期快速返回 WAIT_TIMEOUT 零数据量）
    fn acquire(
        &mut self,
        timeout_ms: u32,
    ) -> windows::core::Result<(FrameKind, ID3D11Texture2D, PixelFormat)> {
        match self {
            AnyCapturer::Dda(c) => {
                let (info, tex, fmt) = c.acquire_frame_timeout(timeout_ms)?;
                Ok((FrameKind::Dda(info), tex, fmt))
            }
            AnyCapturer::Wgc(c) => {
                let (info, tex, fmt) = c.acquire_frame_timeout(timeout_ms)?;
                Ok((FrameKind::Wgc(info), tex, fmt))
            }
        }
    }

    /// GPU 拷贝提交后归还帧（DDA → ReleaseFrame；WGC → drop 归还池缓冲，
    /// 池深 2，持帧不还会耗尽停帧）
    fn release_frame(&mut self) {
        match self {
            AnyCapturer::Dda(c) => c.release_frame(),
            AnyCapturer::Wgc(c) => c.release_frame(),
        }
    }

    /// D3D11 设备引用（RecordGpu 建立在 capture 同设备上——跨设备无法
    /// CopyResource 帧纹理）
    fn device(&self) -> &ID3D11Device {
        match self {
            AnyCapturer::Dda(c) => c.device(),
            AnyCapturer::Wgc(c) => c.device(),
        }
    }

    /// immediate context 引用（record 模块经 Mutex 串行化共享访问）
    fn context(&self) -> &ID3D11DeviceContext {
        match self {
            AnyCapturer::Dda(c) => c.context(),
            AnyCapturer::Wgc(c) => c.context(),
        }
    }

    /// 失效后重建采集管线（DDA → recreate_duplication；WGC → recreate；
    /// 均复用同一 D3D11 设备，GPU 管线无需重建）
    fn recreate(&mut self) -> windows::core::Result<()> {
        match self {
            AnyCapturer::Dda(c) => c.recreate_duplication(),
            AnyCapturer::Wgc(c) => c.recreate(),
        }
    }

    fn backend_name(&self) -> &'static str {
        match self {
            AnyCapturer::Dda(_) => "dda",
            AnyCapturer::Wgc(_) => "wgc",
        }
    }
}

/// 录制区域（虚拟屏幕物理像素坐标；None = 全屏）
#[derive(Clone, Copy, Debug, serde::Deserialize)]
pub struct RecordRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

// ============================================================================
// 队列：票据（线程A→B）与时间环形缓冲（线程B→C）
// ============================================================================

/// 线程 A → 线程 B 的槽位票据（容量 = staging 池深）
pub struct TicketQueue {
    inner: Mutex<VecDeque<Ticket>>,
    cond: Condvar,
    closed: AtomicBool,
}

#[derive(Clone, Copy)]
pub struct Ticket {
    pub slot: usize,
    pub pts_ms: u64,
    /// 全局单调序号（线程 A 分配）：读回 worker 池并行处理后，
    /// 组装器按序重排（去重/入环必须保序）
    pub seq: u64,
}

impl TicketQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// 推票据；队列满（=池深）则兜底阻塞等线程 B 消费。
    /// 返回 false = stop/closed 后放弃（线程 A 随即退出）。
    pub fn push(&self, t: Ticket, stop: &AtomicBool) -> bool {
        let mut q = self.inner.lock().unwrap();
        while q.len() >= STAGING_POOL_DEPTH {
            if stop.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
                return false;
            }
            let (q2, _) = self
                .cond
                .wait_timeout(q, Duration::from_millis(20))
                .unwrap();
            q = q2;
        }
        q.push_back(t);
        drop(q);
        self.cond.notify_all();
        true
    }

    /// 拉票据；None = 已关闭且排空（线程 B 正常退出条件）
    pub fn pop(&self) -> Option<Ticket> {
        let mut q = self.inner.lock().unwrap();
        loop {
            if let Some(t) = q.pop_front() {
                drop(q);
                self.cond.notify_all(); // 唤醒可能因满阻塞的 push
                return Some(t);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            let (q2, _) = self
                .cond
                .wait_timeout(q, Duration::from_millis(50))
                .unwrap();
            q = q2;
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cond.notify_all();
    }
}

/// 环形缓冲帧（PQ16 RGB 紧密 6B/px；高水位时 zstd-1 压缩）
pub struct Frame {
    pub data: Vec<u8>,
    pub compressed: bool,
    pub pts_ms: u64,
}

/// 时间环形缓冲（线程 B → 线程 C）
///
/// 语义 = "保留最近 30 秒"：字节超保险丝或时间超限时丢最旧（正常路径
/// 线程 A 自身 30s 停止，淘汰仅是估算失手的保险丝，evicted 计数告警）。
pub struct FrameRing {
    inner: Mutex<VecDeque<Frame>>,
    cond: Condvar,
    closed: AtomicBool,
    max_bytes: usize,
    total_bytes: std::sync::atomic::AtomicU64,
    evicted: std::sync::atomic::AtomicU64,
}

impl FrameRing {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
            closed: AtomicBool::new(false),
            max_bytes: max_bytes.max(64 * 1024 * 1024),
            total_bytes: std::sync::atomic::AtomicU64::new(0),
            evicted: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn push(&self, frame: Frame) {
        let mut q = self.inner.lock().unwrap();
        let flen = frame.data.len() as u64;
        // 字节保险丝：丢最旧直到放得下
        while self.total_bytes.load(Ordering::Relaxed) + flen > self.max_bytes as u64
            && !q.is_empty()
        {
            if let Some(old) = q.pop_front() {
                self.total_bytes
                    .fetch_sub(old.data.len() as u64, Ordering::Relaxed);
                self.evicted.fetch_add(1, Ordering::Relaxed);
            }
        }
        // 时间环形：超 30s 的最旧帧淘汰（保险丝）
        while let Some(old) = q.front() {
            if frame.pts_ms.saturating_sub(old.pts_ms) > MAX_RECORD_MS {
                let old = q.pop_front().unwrap();
                self.total_bytes
                    .fetch_sub(old.data.len() as u64, Ordering::Relaxed);
                self.evicted.fetch_add(1, Ordering::Relaxed);
            } else {
                break;
            }
        }
        self.total_bytes.fetch_add(flen, Ordering::Relaxed);
        q.push_back(frame);
        drop(q);
        self.cond.notify_all();
    }

    /// 拉帧；None = 已关闭且排空（线程 C 正常退出条件）
    pub fn pop(&self) -> Option<Frame> {
        let mut q = self.inner.lock().unwrap();
        loop {
            if let Some(f) = q.pop_front() {
                self.total_bytes
                    .fetch_sub(f.data.len() as u64, Ordering::Relaxed);
                drop(q);
                self.cond.notify_all();
                return Some(f);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            let (q2, _) = self
                .cond
                .wait_timeout(q, Duration::from_millis(50))
                .unwrap();
            q = q2;
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cond.notify_all();
    }

    pub fn bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// 水位（0..1；线程 B 压缩决策依据）
    pub fn utilization(&self) -> f64 {
        self.bytes() as f64 / self.max_bytes as f64
    }

    pub fn evicted(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }
}

// ============================================================================
// 会话共享状态
// ============================================================================

/// 实时统计（前端轮询 / record://stats 事件负载）
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct StatsPayload {
    pub recording: bool,
    pub elapsed_ms: u64,
    pub captured_frames: u64,
    pub deduped_frames: u64,
    pub encoded_frames: u64,
    pub ring_bytes: u64,
    pub compression_active: bool,
    pub evicted_frames: u64,
    pub output_path: String,
}

/// 录制结果（record://done 事件与 record_stop 返回值）
#[derive(Clone, Debug, serde::Serialize)]
pub struct RecordResult {
    pub path: String,
    pub frames: u64,
    pub duration_ms: u64,
    pub deduped_frames: u64,
    pub evicted_frames: u64,
    pub output_bytes: u64,
    pub auto_stopped: bool,
    pub cancelled: bool,
}

pub struct Shared {
    pub stop: AtomicBool,
    pub cancel: AtomicBool,
    pub auto_stopped: AtomicBool,
    /// 首个致命错误（抓帧/回拷/编码任一线程）
    pub error: Mutex<Option<String>>,
    /// 结束时刻 ms（线程 A 退出时、关闭票据前记录；线程 C 收尾依赖）
    pub end_pts_ms: Mutex<Option<u64>>,
    pub stats: Mutex<StatsCounters>,
    /// 编码线程产出（record_stop 读取）
    pub result: Mutex<Option<anyhow::Result<RecordResult>>>,
}

#[derive(Default)]
pub struct StatsCounters {
    pub elapsed_ms: u64,
    pub captured_frames: u64,
    pub deduped_frames: u64,
    pub encoded_frames: u64,
    pub compression_active: bool,
}

impl Shared {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            auto_stopped: AtomicBool::new(false),
            error: Mutex::new(None),
            end_pts_ms: Mutex::new(None),
            stats: Mutex::new(StatsCounters::default()),
            result: Mutex::new(None),
        }
    }

    /// 统计快照（合并 ring 的水位/淘汰计数）
    pub fn snapshot(&self, ring: &FrameRing, path: &str, recording: bool) -> StatsPayload {
        let s = self.stats.lock().unwrap();
        StatsPayload {
            recording,
            elapsed_ms: s.elapsed_ms,
            captured_frames: s.captured_frames,
            deduped_frames: s.deduped_frames,
            encoded_frames: s.encoded_frames,
            ring_bytes: ring.bytes(),
            compression_active: s.compression_active,
            evicted_frames: ring.evicted(),
            output_path: path.to_string(),
        }
    }
}

/// 会话句柄（commands 持有；stop 时 join 三线程取结果）
pub struct Session {
    pub shared: Arc<Shared>,
    pub ring: Arc<FrameRing>,
    pub capture_handle: Option<std::thread::JoinHandle<()>>,
    pub inflight_handle: Option<std::thread::JoinHandle<()>>,
    /// 实时模式：启动即存在；纯录制模式：停止后由 finish_session 补启
    pub encode_handle: Option<std::thread::JoinHandle<()>>,
    pub output_path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 纯录制模式标记（finish_session 据此补启编码线程）
    pub deferred_encode: bool,
    /// 会话质量档（延迟补启编码时使用启动时的值，避免配置中途被改）
    pub quality: QualityLevel,
    /// app 句柄（延迟编码补启时事件发射用）
    pub app: tauri::AppHandle,
}

// ============================================================================
// 会话启动
// ============================================================================

pub struct SpawnParams {
    pub region: Option<RecordRegion>,
    pub quality: QualityLevel,
    /// None = 自适应（一律 30fps；内存预算公式见设计文档 §3.6）
    pub fps: Option<u32>,
    /// 录制时长上限（秒；会话启动时从 config 读取定格，钳制 1..=30——
    /// 运行时上限 = max_seconds*1000，绝对上限仍为 MAX_RECORD_MS）
    pub max_seconds: u32,
    pub output_path: PathBuf,
    /// 环形缓冲字节保险丝
    pub ring_bytes: usize,
    /// zstd 水位阈值（0..1）
    pub zstd_watermark: f64,
    /// 纯录制模式（游戏场景）：录制期间不启动编码线程，CPU 全归前台应用；
    /// 环形缓冲全程 zstd（不赶时间换内存安全），停止后 NORMAL 优先级全核编码。
    pub deferred_encode: bool,
    /// 采集后端（commands 已按 config×osd 解析：Auto 仅游戏模式传入——
    /// spawn_session 内 WGC 优先 + DDA 兜底；桌面/强制路径直接 Wgc/Dda）
    pub capture_backend: CaptureBackend,
    pub app: tauri::AppHandle,
}

/// 启动录制会话（三线程立即返回；输出文件由编码线程创建）
pub fn spawn_session(p: SpawnParams) -> anyhow::Result<Session> {
    // 1. 显示器选择：区域中心所在显示器 / 主显示器（全屏）
    let monitors = enumerate_monitors()?;
    let monitor = match p.region {
        Some(r) => {
            let cx = r.x + r.width as i32 / 2;
            let cy = r.y + r.height as i32 / 2;
            monitors
                .iter()
                .find(|m| cx >= m.left && cx < m.right && cy >= m.top && cy < m.bottom)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("录制区域中心不在任何显示器上"))?
        }
        None => monitors
            .iter()
            .find(|m| m.left == 0 && m.top == 0)
            .cloned()
            .or_else(|| monitors.first().cloned())
            .ok_or_else(|| anyhow::anyhow!("未找到可用显示器"))?,
    };

    // 2. 裁剪偏移与输出尺寸（区域与显示器求交，转 DDA 纹理坐标）
    let (crop, out_w, out_h) = match p.region {
        Some(r) => {
            let x0 = r.x.max(monitor.left) - monitor.left;
            let y0 = r.y.max(monitor.top) - monitor.top;
            let x1 = (r.x + r.width as i32).min(monitor.right) - monitor.left;
            let y1 = (r.y + r.height as i32).min(monitor.bottom) - monitor.top;
            if x1 <= x0 || y1 <= y0 {
                anyhow::bail!("录制区域为空（与显示器无交集）");
            }
            ((x0 as u32, y0 as u32), (x1 - x0) as u32, (y1 - y0) as u32)
        }
        None => ((0, 0), monitor.width, monitor.height),
    };

    // 3. fps 自适应（auto：一律 30）
    //    回放填充速率实测 27.6fps（native 解码腿 36ms/帧）：60fps 录制产物
    //    回放时播放头永远追不上填充前沿（滑窗模式），观感"边播边卡"；
    //    30fps 与填充速率匹配，符合录屏演示场景，且文件体积小约 30%。
    //    原 ≥4K→30 档保持，<4K 由 60 降为 30。
    let fps = p.fps.unwrap_or(30);

    // 4. 采集器（双后端）+ GPU 预处理管线（同设备）
    //    - Wgc：强制 WGC，失败即报错
    //    - Dda：强制 DDA（原路径）
    //    - Auto：WGC 优先（游戏模式主路径：挂钩 DWM 呈现层，免疫
    //      independent flip / MPO 平面重排）；for_monitor 失败 log warn
    //      "WGC 不可用" 回退 DDA
    //    WGC 构造线程须先 MTA（WinRT 激活前提；幂等——仅 WGC 路径调用，
    //    不污染纯 DDA 路径线程的 COM 状态）
    let (capturer, backend_desc): (AnyCapturer, String) = match p.capture_backend {
        CaptureBackend::Wgc => {
            crate::capture::wgc::ensure_mta()
                .map_err(|e| anyhow::anyhow!("WGC MTA 初始化失败: {}", e))?;
            let cap = WgcCapturer::for_monitor(&monitor)
                .map_err(|e| anyhow::anyhow!("WGC 捕获器创建失败: {}", e))?;
            (AnyCapturer::Wgc(cap), "wgc".to_string())
        }
        CaptureBackend::Dda => (
            AnyCapturer::Dda(DesktopCapturer::for_monitor(&monitor, true)?),
            "dda".to_string(),
        ),
        CaptureBackend::Auto => {
            match crate::capture::wgc::ensure_mta()
                .and_then(|()| WgcCapturer::for_monitor(&monitor))
            {
                Ok(cap) => (AnyCapturer::Wgc(cap), "wgc(dda)".to_string()),
                Err(e) => {
                    log::warn!("[record] WGC 不可用，回退 DDA: {}", e);
                    (
                        AnyCapturer::Dda(DesktopCapturer::for_monitor(&monitor, true)?),
                        "dda".to_string(),
                    )
                }
            }
        }
    };
    let gpu = Arc::new(RecordGpu::new(
        capturer.device(),
        capturer.context().clone(),
        monitor.width,
        monitor.height,
        crop,
        out_w,
        out_h,
    )?);

    // 运行时长上限（会话启动时从 config 定格；钳制 1..=30 = MAX_RECORD_MS 绝对上限内）
    let record_max_ms =
        (p.max_seconds.clamp(1, (MAX_RECORD_MS / 1000) as u32) as u64) * 1000;

    let shared = Arc::new(Shared::new());
    let tickets = Arc::new(TicketQueue::new());
    let ring = Arc::new(FrameRing::new(p.ring_bytes));

    // 5. 三线程（纯录制模式：编码线程延迟到停止后启动）
    let capture_handle = {
        let shared = shared.clone();
        let tickets = tickets.clone();
        let ring = ring.clone();
        let gpu = gpu.clone();
        let app = p.app.clone();
        // WGC 帧尺寸校验基准（生产 DPI 感知进程 = 物理像素 = monitor 尺寸）
        let expected_full = (monitor.width, monitor.height);
        std::thread::Builder::new()
            .name("record-capture".into())
            .spawn(move || {
                capture_loop(
                    capturer,
                    gpu,
                    tickets,
                    ring,
                    shared,
                    fps,
                    record_max_ms,
                    app,
                    expected_full,
                );
            })?
    };
    let inflight_handle = {
        let shared = shared.clone();
        let tickets = tickets.clone();
        let ring = ring.clone();
        let gpu = gpu.clone();
        // 纯录制模式全程 zstd（水位 0 = 恒压缩）：不赶编码时间，换内存安全
        let watermark = if p.deferred_encode {
            0.0
        } else {
            p.zstd_watermark.clamp(0.05, 1.0)
        };
        std::thread::Builder::new()
            .name("record-inflight".into())
            .spawn(move || {
                inflight_loop(gpu, tickets, ring, shared, watermark);
            })?
    };
    let encode_handle = if p.deferred_encode {
        None // 停止后由 commands::finish_session 启动（NORMAL 优先级全核）
    } else {
        let shared = shared.clone();
        let ring = ring.clone();
        let path = p.output_path.clone();
        let app = p.app.clone();
        let quality = p.quality;
        Some(
            std::thread::Builder::new()
                .name("record-encode".into())
                .spawn(move || {
                    encode_loop(ring, shared, path, out_w, out_h, quality, app, true);
                })?,
        )
    };

    log::info!(
        "[record] 会话启动：{}x{}（区域偏移 {:?}）fps={} 质量={:?} 后端={} 上限={}s 保险丝={}MB 水位={:.2} 模式={}",
        out_w,
        out_h,
        crop,
        fps,
        p.quality,
        backend_desc,
        record_max_ms / 1000,
        p.ring_bytes / 1024 / 1024,
        p.zstd_watermark,
        if p.deferred_encode {
            "纯录制(延迟编码)"
        } else {
            "实时编码"
        }
    );

    Ok(Session {
        shared,
        ring,
        capture_handle: Some(capture_handle),
        inflight_handle: Some(inflight_handle),
        encode_handle,
        output_path: p.output_path,
        width: out_w,
        height: out_h,
        fps,
        deferred_encode: p.deferred_encode,
        quality: p.quality,
        app: p.app,
    })
}

// ============================================================================
// 线程 A：抓帧
// ============================================================================

fn capture_loop(
    mut cap: AnyCapturer,
    gpu: Arc<RecordGpu>,
    tickets: Arc<TicketQueue>,
    ring: Arc<FrameRing>,
    shared: Arc<Shared>,
    fps: u32,
    record_max_ms: u64,
    app: tauri::AppHandle,
    // 期望全屏纹理尺寸（WGC 帧尺寸校验：生产 DPI 感知进程 =
    // monitor.width/height 物理像素；DPI 异常/模式切换黑边帧安全丢弃）
    expected_full: (u32, u32),
) {
    use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT};

    // panic 保险：线程意外终止（如锁毒化）也关闭票据，防线程 B/record_stop 悬挂
    struct CloseTickets<'a>(&'a TicketQueue);
    impl Drop for CloseTickets<'_> {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _close_guard = CloseTickets(&tickets);

    // WGC 需要 MTA（WinRT 线程前提；capture 线程专用，构造线程的初始化
    // 不跟随线程迁移）；DDA 不需要——失败仅告警继续（幂等无害）
    if let Err(e) = crate::capture::wgc::ensure_mta() {
        log::warn!(
            "[record] capture 线程 MTA 初始化失败（WGC 受影响，DDA 不受影响）：{}",
            e
        );
    }

    let start = Instant::now();
    let min_interval = Duration::from_millis((1000 / fps.max(1)) as u64);
    let mut last_accept: Option<Instant> = None;
    let mut slot: usize = 0;
    let mut first = true;
    let mut last_emit = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut err: Option<String> = None;
    // 帧到达间隔诊断（DWM/采集层交付时刻差；与产物帧间隔对照分辨源头/消费丢帧）
    let mut arrival_prev: Option<i64> = None;
    let mut arrival_diffs: Vec<u64> = Vec::new();
    let mut process_us_total: u128 = 0;
    let mut process_count: u64 = 0;
    // acquire 计时：前 10 次进出时间（定位 AcquireNextFrame 悬挂）
    static ACQ_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    // 票据序号（读回 worker 池按序组装用；会话开始重置——SESSION 保证同一
    // 时刻仅一个录制会话，无并发竞争）
    static TICKET_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    TICKET_SEQ.store(0, std::sync::atomic::Ordering::Relaxed);
    let ticket_seq = &TICKET_SEQ;
    log::info!(
        "[record] 抓帧线程启动: 后端={} fps={} min_interval={}ms",
        cap.backend_name(),
        fps,
        min_interval.as_millis()
    );

    'capture: loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let elapsed_ms = start.elapsed().as_millis() as u64;
        if elapsed_ms >= record_max_ms {
            shared.auto_stopped.store(true, Ordering::Release);
            log::info!("[record] 达到 {}s 上限，自动停止", record_max_ms / 1000);
            let _ = app.emit("record://autostop", ());
            break;
        }
        shared.stats.lock().unwrap().elapsed_ms = elapsed_ms;

        // 统计每秒推送（无论是否有新帧——桌面静止也要走表，否则 OSD 计时冻在 0:00）
        if last_emit.elapsed() >= Duration::from_secs(1) {
            last_emit = Instant::now();
            emit_stats(&shared, &ring, &app);
        }

        // 出帧（短超时；桌面静止 = 超时零数据量，机会式编码空档——
        // 双后端语义一致：DDA 静止超时 / WGC 静止桌面出完首帧快照后不再交付）
        let acq_n = ACQ_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let acq_t0 = std::time::Instant::now();
        if acq_n < 10 {
            log::info!("[record] acquire #{} 进入", acq_n + 1);
        }
        let (kind, tex, _fmt) = match cap.acquire(50) {
            Ok(v) => {
                if acq_n < 10 {
                    log::info!(
                        "[record] acquire #{} 返回 用时={}ms",
                        acq_n + 1,
                        acq_t0.elapsed().as_millis()
                    );
                }
                v
            }
            Err(e) => {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    // 静止心跳：>5s 无任何帧时每 5s 打一条（正常去重行为，防"假死"误判）
                    if last_heartbeat.elapsed() >= Duration::from_secs(5) {
                        last_heartbeat = Instant::now();
                        log::info!(
                            "[record] 桌面静止待帧 {}s（已捕获 {} 帧）",
                            elapsed_ms / 1000,
                            shared.stats.lock().unwrap().captured_frames
                        );
                    }
                    continue;
                }
                // ACCESS_LOST（DDA：MPO 平面重排/独占全屏切换/桌面合成器重置；
                // WGC：item Closed——显示器模式切换/移除）：采集管线已失效但
                // 设备完好——线程内重建后继续抓帧，而非秒死收尾出半成品。
                // 重建复用同一 D3D11 设备，GPU 管线不受影响。
                if e.code() == DXGI_ERROR_ACCESS_LOST {
                    log::warn!(
                        "[record] {} ACCESS_LOST（已捕获 {} 帧），重建采集管线重试",
                        cap.backend_name(),
                        shared.stats.lock().unwrap().captured_frames
                    );
                    let mut rebuilt = false;
                    for attempt in 1..=ACCESS_LOST_REBUILD_MAX {
                        // 重建等待期间 stop/时长上限检查照常（重试循环内）
                        if shared.stop.load(Ordering::Acquire) {
                            break 'capture;
                        }
                        if start.elapsed().as_millis() as u64 >= record_max_ms {
                            shared.auto_stopped.store(true, Ordering::Release);
                            log::info!(
                                "[record] 达到 {}s 上限，自动停止",
                                record_max_ms / 1000
                            );
                            let _ = app.emit("record://autostop", ());
                            break 'capture;
                        }
                        match cap.recreate() {
                            Ok(()) => {
                                rebuilt = true;
                                log::info!(
                                    "[record] 采集管线重建成功（第 {} 次尝试），继续抓帧",
                                    attempt
                                );
                                break;
                            }
                            Err(re) => {
                                log::warn!(
                                    "[record] 采集管线重建失败（{}/{}）：{}",
                                    attempt,
                                    ACCESS_LOST_REBUILD_MAX,
                                    re
                                );
                                if attempt < ACCESS_LOST_REBUILD_MAX {
                                    std::thread::sleep(Duration::from_millis(
                                        ACCESS_LOST_RETRY_INTERVAL_MS,
                                    ));
                                }
                            }
                        }
                    }
                    if !rebuilt {
                        err = Some(format!(
                            "{} ACCESS_LOST 后连续 {} 次重建采集管线失败: {}",
                            cap.backend_name(),
                            ACCESS_LOST_REBUILD_MAX,
                            e
                        ));
                        break;
                    }
                    // 重建后首帧 = 新采集管线的当前桌面累积内容（与录制
                    // 首帧同语义），设 first 强制接受防黑帧；start 不重置，
                    // pts 时间轴连续不跳变。
                    first = true;
                    continue;
                }
                // 其他错误码（设备移除等）：致命
                err = Some(format!("{} 帧获取失败: {}", cap.backend_name(), e));
                break;
            }
        };

        // 纯鼠标帧（画面未变；光标不入镜）→ 丢弃（仅 DDA 有此概念：
        // LastPresentTime==0）。首帧例外：duplication 初始帧即当前桌面累积
        // 内容。WGC 帧全接受（cursor 已禁，无纯鼠标帧）。
        // 诊断：帧到达间隔（DWM/采集层交付时刻差，QPC 100ns→ms）——
        // 与最终产物帧间隔对照可分辨"源头低帧率"vs"消费链丢帧"。
        if let Some(p) = arrival_prev {
            let ts = match &kind {
                FrameKind::Dda(i) => i.LastPresentTime,
                FrameKind::Wgc(w) => w.system_relative_time,
            };
            if ts > p {
                arrival_diffs.push(((ts - p) / 10_000) as u64);
            }
        }
        match &kind {
            FrameKind::Dda(info) if !first && info.LastPresentTime == 0 => {
                cap.release_frame();
                continue;
            }
            _ => {}
        }
        if let FrameKind::Dda(i) = &kind {
            if i.LastPresentTime != 0 {
                arrival_prev = Some(i.LastPresentTime);
            }
        } else if let FrameKind::Wgc(w) = &kind {
            arrival_prev = Some(w.system_relative_time);
        }

        // WGC 帧尺寸校验：帧纹理 = 物理像素（生产 DPI 感知进程 =
        // monitor.width/height）；不符 = DPI 异常/模式切换黑边帧 →
        // 安全丢弃（RecordGpu 的裁剪坐标按 monitor 尺寸建立，尺寸漂移不可信）
        if matches!(kind, FrameKind::Wgc(_)) {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            if (desc.Width, desc.Height) != expected_full {
                log::warn!(
                    "[record] WGC 帧尺寸 {}x{} 与显示器 {}x{} 不符，丢弃（DPI 异常/模式切换黑边帧）",
                    desc.Width, desc.Height, expected_full.0, expected_full.1
                );
                cap.release_frame();
                continue;
            }
        }
        // fps 上限限流
        if let Some(la) = last_accept {
            if la.elapsed() < min_interval {
                cap.release_frame();
                continue;
            }
        }

        // GPU 预处理（裁剪+转换+哈希 → staging 池提交）+ 立即归还采集队列
        // （DDA ReleaseFrame / WGC 归还池缓冲；时序约定：GPU 拷贝提交后才 release）
        let pts_ms = start.elapsed().as_millis() as u64;
        let t_proc = Instant::now();
        if let Err(e) = gpu.process_frame(&tex, slot) {
            cap.release_frame();
            err = Some(format!("GPU 预处理失败: {:#}", e));
            break;
        }
        process_us_total += t_proc.elapsed().as_micros();
        process_count += 1;
        cap.release_frame();
        drop(tex);

        // 推票据（满则兜底阻塞；stop 放弃）
        if !tickets.push(
            Ticket {
                slot,
                pts_ms,
                seq: ticket_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            },
            &shared.stop,
        ) {
            break;
        }
        slot = (slot + 1) % STAGING_POOL_DEPTH;
        last_accept = Some(Instant::now());
        first = false;
        let mut s = shared.stats.lock().unwrap();
        s.captured_frames += 1;
        if s.captured_frames == 1 {
            log::info!("[record] 首帧已捕获（进入正常录制）");
        }
        drop(s);
    }

    // 结束时刻先记录、后关票据（线程 C 收尾依赖 end_pts 已就绪）
    let end_ms = start.elapsed().as_millis() as u64;
    *shared.end_pts_ms.lock().unwrap() = Some(end_ms);
    // 到达间隔诊断：源头帧率 vs 消费帧率 vs GPU 预处理耗时
    if !arrival_diffs.is_empty() {
        let mut d = arrival_diffs.clone();
        d.sort();
        let n = d.len();
        log::info!(
            "[record] 采集诊断: 采集层交付间隔 min/p50/p90/max = {}/{}/{}/{}ms（{} 个样本，≈{:.1}fps）",
            d[0], d[n / 2], d[n * 90 / 100], d[n - 1],
            n, 1000.0 / d[n / 2].max(1) as f64
        );
    }
    if process_count > 0 {
        log::info!(
            "[record] 采集诊断: GPU 预处理均值 {:.1}ms（{} 帧）",
            process_us_total as f64 / 1000.0 / process_count as f64,
            process_count
        );
    }
    log::info!(
        "[record] 抓帧线程退出: end={}ms 已捕获={} 帧 err={:?}",
        end_ms,
        shared.stats.lock().unwrap().captured_frames,
        err
    );
    if let Some(e) = err {
        log::error!("[record] 抓帧线程终止: {}", e);
        *shared.error.lock().unwrap() = Some(e.clone());
        // 线程 B 可能因本线程错误退出 → 确保 push 不悬挂
        shared.stop.store(true, Ordering::Release);
        // 僵尸会话修复：通知前端复位（record://error）+ 后台自动收尾
        // （否则统计停推 OSD 冻在 00:00、30s 自动停失效、会话残留到手动停止）
        super::commands::autofinish_capture_error(&app, e.clone());
    }
    tickets.close();
    emit_stats(&shared, &ring, &app);
}

// ============================================================================
// 线程 B：回拷 + 水位自适应压缩 + 入环
// ============================================================================

/// 读回 worker 的产出（乱序送达，组装器按 seq 重排）
struct InflightItem {
    seq: u64,
    hash: u64,
    pts_ms: u64,
    pq16: Vec<u8>,
}

/// 读回 worker：拉票据 → 哈希 + 大回拷（31MB DMA + 并行 swizzle）→ 送组装器。
/// 多 worker 并行使 DMA 与 CPU 拷贝流水重叠（单线程 ~80ms/帧 → 12fps 瓶颈，
/// 3 worker 实测可达 60fps 消费）。
fn inflight_worker(
    gpu: Arc<RecordGpu>,
    tickets: Arc<TicketQueue>,
    tx: std::sync::mpsc::Sender<InflightItem>,
    shared: Arc<Shared>,
    seq_log: &std::sync::atomic::AtomicUsize,
) {
    loop {
        let ticket = match tickets.pop() {
            Some(t) => t,
            None => break, // 票据关闭且排空
        };
        let b_n = seq_log.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if b_n < 5 {
            log::info!("[record] 线程B 票据#{} slot={} seq={}", b_n + 1, ticket.slot, ticket.seq);
        }
        // GPU 哈希（组装器统一去重比较——worker 间乱序，此处不做前帧比较）
        let hash = match gpu.read_hash(ticket.slot) {
            Ok(h) => h,
            Err(e) => {
                inflight_fatal(&shared, format!("哈希回读失败: {:#}", e));
                // 补发丢弃标记：保持 seq 连续（组装器见空 Vec 跳过），
                // 否则后续帧永久滞留重排缓冲不入环
                let _ = tx.send(InflightItem {
                    seq: ticket.seq,
                    hash: 0,
                    pts_ms: ticket.pts_ms,
                    pq16: Vec::new(),
                });
                break;
            }
        };
        // 大回拷（PQ16 RGBA16 → 紧密 RGB 6B/px；内部 4 线程并行 swizzle）
        let pq16 = match gpu.read_pq16(ticket.slot) {
            Ok(d) => d,
            Err(e) => {
                inflight_fatal(&shared, format!("帧回读失败: {:#}", e));
                let _ = tx.send(InflightItem {
                    seq: ticket.seq,
                    hash: 0,
                    pts_ms: ticket.pts_ms,
                    pq16: Vec::new(),
                });
                break;
            }
        };
        if tx
            .send(InflightItem {
                seq: ticket.seq,
                hash,
                pts_ms: ticket.pts_ms,
                pq16,
            })
            .is_err()
        {
            break; // 组装器已退出（异常收尾）
        }
    }
}

/// 线程 B 主入口：worker 池（3）+ 本线程为有序组装器。
/// 组装器按 seq 重排后做去重比较 / 水位 zstd / 入环（保序语义不变）。
/// 对外仍是单 JoinHandle：组装器退出 = worker 全部 join 完成 = 环形已关闭。
fn inflight_loop(
    gpu: Arc<RecordGpu>,
    tickets: Arc<TicketQueue>,
    ring: Arc<FrameRing>,
    shared: Arc<Shared>,
    watermark: f64,
) {
    // panic 保险：组装器线程意外终止也关闭环形，防线程 C/record_stop 悬挂
    struct CloseRing<'a>(&'a FrameRing);
    impl Drop for CloseRing<'_> {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _close_guard = CloseRing(&ring);
    // worker 公用首几帧日志序号
    static B_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    B_SEQ.store(0, std::sync::atomic::Ordering::Relaxed);

    const INFLIGHT_WORKERS: usize = 3;
    let (tx, rx) = std::sync::mpsc::channel::<InflightItem>();
    let mut workers = Vec::with_capacity(INFLIGHT_WORKERS);
    for _ in 0..INFLIGHT_WORKERS {
        let gpu = gpu.clone();
        let tickets = tickets.clone();
        let tx = tx.clone();
        let shared = shared.clone();
        workers.push(
            std::thread::Builder::new()
                .name("record-inflight".into())
                .spawn(move || inflight_worker(gpu, tickets, tx, shared, &B_SEQ))
                .expect("spawn inflight worker"),
        );
    }
    drop(tx); // 本线程不生产；全部 worker 退出后 rx 迭代自然结束

    // 有序组装：seq 单调（0 起），乱序结果缓冲 BTreeMap
    let mut reorder: std::collections::BTreeMap<u64, InflightItem> =
        std::collections::BTreeMap::new();
    let mut next_seq: u64 = 0;
    let mut last_hash: Option<u64> = None;
    for item in rx {
        reorder.insert(item.seq, item);
        // 按序冲刷（seq 连续段全部出队）
        while let Some(it) = reorder.remove(&next_seq) {
            next_seq += 1;
            // 丢弃标记（worker fatal 补发）：跳过不入环
            if it.pq16.is_empty() {
                log::warn!("[record] 组装器跳过帧 seq={}（读回失败丢弃标记）", it.seq);
                continue;
            }
            if last_hash == Some(it.hash) {
                shared.stats.lock().unwrap().deduped_frames += 1;
                continue;
            }
            // 水位自适应压缩：低水位零成本直入；高水位 zstd-1（滞后触发）
            let compress = ring.utilization() >= watermark;
            let (data, compressed) = if compress {
                match zstd::bulk::compress(&it.pq16, 1) {
                    Ok(c) => (c, true),
                    Err(e) => {
                        log::warn!("[record] zstd 压缩失败（{}），本帧原始入环", e);
                        (it.pq16, false)
                    }
                }
            } else {
                (it.pq16, false)
            };
            {
                let mut s = shared.stats.lock().unwrap();
                s.compression_active = compress;
            }
            ring.push(Frame {
                data,
                compressed,
                pts_ms: it.pts_ms,
            });
            last_hash = Some(it.hash);
        }
    }
    // worker 全部退出（含 fatal 路径）——join 清理
    for w in workers {
        let _ = w.join();
    }
    log::info!("[record] 回拷线程退出: 环形水位={:.2}", ring.utilization());
    ring.close();
}

/// 线程 B 致命错误：记录 + 置 stop（防线程 A push 悬挂）后退出
fn inflight_fatal(shared: &Arc<Shared>, msg: String) {
    log::error!("[record] 回拷线程终止: {}", msg);
    *shared.error.lock().unwrap() = Some(msg);
    shared.stop.store(true, Ordering::Release);
}

// ============================================================================
// 线程 C：编码
// ============================================================================

fn encode_loop(
    ring: Arc<FrameRing>,
    shared: Arc<Shared>,
    path: PathBuf,
    width: u32,
    height: u32,
    quality: QualityLevel,
    app: tauri::AppHandle,
    below_normal: bool,
) {
    if below_normal {
        set_thread_priority(false);
    }
    // 纯录制模式（below_normal=false）：停止后启动，NORMAL 优先级全核——
    // 此时录制已结束，不再与前台应用争抢
    //
    // panic 防护：编码线程 unwind 死亡时产物必然截断在最后写入处（无 finalize），
    // 且无此防护时 shared.result 永不写入（record_stop 报"未返回结果"）、无
    // record://error 事件——损坏文件静默留存。捕获后转为错误结果上报。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        encode_inner(&ring, &shared, &path, width, height, quality)
    }));
    let res = match result {
        Ok(Ok(mut r)) => {
            r.auto_stopped = shared.auto_stopped.load(Ordering::Acquire);
            log::info!(
                "[record] 完成：{}（{} 帧 / {}ms / {} 字节，去重 {} 淘汰 {}）",
                r.path,
                r.frames,
                r.duration_ms,
                r.output_bytes,
                r.deduped_frames,
                r.evicted_frames
            );
            let _ = app.emit("record://done", &r);
            Ok(r)
        }
        Ok(Err(e)) => {
            log::error!("[record] 编码线程失败: {:#}", e);
            let _ = app.emit("record://error", format!("{:#}", e));
            Err(e)
        }
        Err(panic) => {
            let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else {
                "未知 panic".to_string()
            };
            log::error!("[record] 编码线程 panic（产物截断于最后写入处）: {}", msg);
            let _ = app.emit(
                "record://error",
                format!("编码线程异常终止: {}", msg),
            );
            Err(anyhow::anyhow!("编码线程异常终止: {}", msg))
        }
    };
    *shared.result.lock().unwrap() = Some(res);
}

fn encode_inner(
    ring: &Arc<FrameRing>,
    shared: &Arc<Shared>,
    path: &PathBuf,
    width: u32,
    height: u32,
    quality: QualityLevel,
) -> anyhow::Result<RecordResult> {
    let mut enc =
        AnimationJxlEncoder::new(path, width, height, quality, animation_effort(quality))?;
    let frame_len = width as usize * height as usize * 6;
    let mut encoded: u64 = 0;

    // 延迟一帧定长：pending 的 duration 由下一帧/结束时刻决定
    let mut pending: Option<Frame> = None;
    let mut pending_is_first = true;

    loop {
        let frame = match ring.pop() {
            Some(f) => f,
            None => break,
        };
        if shared.cancel.load(Ordering::Acquire) {
            // 取消：放弃剩余帧（半成品文件由 record_cancel 删除）
            return Ok(RecordResult {
                path: path.to_string_lossy().into_owned(),
                frames: encoded,
                duration_ms: 0,
                deduped_frames: 0,
                evicted_frames: 0,
                output_bytes: 0,
                auto_stopped: false,
                cancelled: true,
            });
        }
        if let Some(p) = pending.take() {
            // 首帧基准 0（动画从 t=0 显示）；其余帧基准 = 自身 pts
            let base = if pending_is_first { 0 } else { p.pts_ms };
            let dur = frame.pts_ms.saturating_sub(base).max(1);
            let bytes = frame_bytes(&p, frame_len);
            enc.add_frame(&bytes, dur, false)?;
            pending_is_first = false;
            encoded += 1;
            shared.stats.lock().unwrap().encoded_frames = encoded;
        }
        pending = Some(frame);
    }

    // 收尾：末帧 duration = 结束时刻 - 基准
    let p = pending.ok_or_else(|| anyhow::anyhow!("录制无有效帧"))?;
    let end_pts = shared.end_pts_ms.lock().unwrap().unwrap_or(p.pts_ms);
    let base = if pending_is_first { 0 } else { p.pts_ms };
    let dur = end_pts.saturating_sub(base).max(1);
    let bytes = frame_bytes(&p, frame_len);
    enc.add_frame(&bytes, dur, true)?;
    enc.finalize()?;

    let output_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Ok(RecordResult {
        path: path.to_string_lossy().into_owned(),
        frames: encoded + 1,
        duration_ms: end_pts,
        deduped_frames: shared.stats.lock().unwrap().deduped_frames,
        evicted_frames: ring.evicted(),
        output_bytes,
        auto_stopped: false,
        cancelled: false,
    })
}

/// 帧数据取出（按需 zstd 解压）
fn frame_bytes(f: &Frame, frame_len: usize) -> std::borrow::Cow<'_, [u8]> {
    if f.compressed {
        match zstd::bulk::decompress(&f.data, frame_len) {
            Ok(d) => std::borrow::Cow::Owned(d),
            Err(e) => {
                log::warn!("[record] zstd 解压失败（{}），原始数据交给编码器", e);
                std::borrow::Cow::Borrowed(&f.data[..])
            }
        }
    } else {
        std::borrow::Cow::Borrowed(&f.data[..])
    }
}

// ============================================================================
// 辅助
// ============================================================================

/// 纯录制模式：停止后补启编码线程（NORMAL 优先级全核）。
/// 调用前提：线程 A/B 已 join（ring 已 close 且数据完整）。
pub fn start_deferred_encode(session: &mut Session) -> anyhow::Result<()> {
    if !session.deferred_encode || session.encode_handle.is_some() {
        anyhow::bail!("仅纯录制模式且未启动编码时可用");
    }
    let shared = session.shared.clone();
    let ring = session.ring.clone();
    let path = session.output_path.clone();
    let app = session.app.clone();
    let (w, h) = (session.width, session.height);
    let quality = session.quality;
    session.encode_handle = Some(
        std::thread::Builder::new()
            .name("record-encode-deferred".into())
            .spawn(move || encode_loop(ring, shared, path, w, h, quality, app, false))?,
    );
    log::info!("[record] 延迟编码启动（NORMAL 优先级全核）");
    Ok(())
}

fn emit_stats(shared: &Shared, ring: &FrameRing, app: &tauri::AppHandle) {
    let payload = shared.snapshot(ring, "", true);
    let _ = app.emit("record://stats", &payload);
}

/// 设置当前线程优先级（below_normal=true 降到 BELOW_NORMAL，false 保持 NORMAL）
fn set_thread_priority(below_normal: bool) {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    unsafe {
        let h = GetCurrentThread();
        if below_normal {
            let _ = SetThreadPriority(h, THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

// ============================================================================
// 集成测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::monitor::enumerate_monitors;

    /// WGC → RecordGpu 全链集成冒烟（W3 集成验收）
    ///
    /// 验证生产 spawn_session 的 WGC 路径三关：
    /// 1. 同设备——RecordGpu 建在 WgcCapturer 自建设备上（CopyResource 可达）；
    /// 2. 同格式——f16（HDR scRGB 线性，p2.w=0 路径直消费）/ Bgra8（SDR）；
    /// 3. 尺寸——输入束跟随 WGC 帧纹理。
    ///
    /// 流程：主屏 WgcCapturer::for_monitor → 取首帧（静止桌面 StartCapture 后
    /// 交付 1 帧快照——W2 实测语义，2000ms 裕量）→ RecordGpu::new（与生产
    /// 同参数形态）→ process_frame → read_hash（staging 槽可读 = compute
    /// 管线真实跑通）→ release_frame。
    #[test]
    fn wgc_record_pipeline_smoke() {
        crate::capture::wgc::ensure_mta().expect("CoInitializeEx(MTA) 失败");
        let monitors = enumerate_monitors().expect("enumerate_monitors 失败");
        let monitor = monitors
            .iter()
            .find(|m| m.left == 0 && m.top == 0)
            .or_else(|| monitors.first())
            .cloned()
            .expect("无显示器");
        println!(
            "[smoke] 主显示器 {}x{} hdr_mode={:?}（测试进程 DPI 未感知时为逻辑分辨率）",
            monitor.width, monitor.height, monitor.hdr_mode
        );

        let mut cap =
            WgcCapturer::for_monitor(&monitor).expect("WgcCapturer::for_monitor 失败");
        // 静止桌面首帧快照（StartCapture 后必交付 1 帧——W2 验证；2000ms 裕量）
        let (info, tex, fmt) = cap
            .acquire_frame_timeout(2000)
            .expect("WGC 首帧获取失败（2000ms；静止桌面应有快照帧）");
        println!(
            "[smoke] WGC 帧: content={:?} fmt={:?} sys_rel={}",
            info.content_size, fmt, info.system_relative_time
        );

        // RecordGpu 建在 WGC 设备上（同设备关；参数形态与生产 spawn_session 一致）
        let gpu = RecordGpu::new(
            cap.device(),
            cap.context().clone(),
            monitor.width,
            monitor.height,
            (0, 0),
            monitor.width,
            monitor.height,
        )
        .expect("RecordGpu::new 失败");
        gpu.process_frame(&tex, 0)
            .expect("process_frame 失败（WGC→RecordGpu 同设备/格式关）");
        // GPU 拷贝已提交 → 归还池缓冲（时序约定与生产 capture_loop 一致）
        cap.release_frame();
        drop(tex);

        // staging 槽 0 哈希可读 = 裁剪+转换+哈希 compute 管线全链跑通
        let hash = gpu.read_hash(0).expect("read_hash 失败（staging 槽回读）");
        println!("[smoke] slot 0 帧哈希 = {:#018x}", hash);
    }
}
