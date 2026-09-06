//! WGC（Windows.Graphics.Capture）采集模块 —— 游戏逐帧采集底层
//!
//! 与 DXGI Desktop Duplication（dxgi_duplication.rs）互补的第二条采集通路：
//! WGC 挂钩 DWM 呈现层，系统主动逐帧推送已合成的桌面内容，
//! **免疫 independent flip / MPO 平面重排**——游戏独占/优化全屏切换、
//! overlay 平面变化时 DDA 会 ACCESS_LOST 或拿到未合成黑帧，WGC 持续稳定。
//!
//! 设计要点：
//! - **格式由 monitor.hdr_mode 决定**（无 prefer_hdr 参数）：
//!   HdrScRgb / Hdr10 → R16G16B16A16_FLOAT；Sdr → B8G8R8A8_UNORM。
//!   **SDR 桌面绝不建 float16 池**：WGC 在 SDR 输出上请求 fp16 时交付的是
//!   sRGB gamma 编码值（而非 scRGB 线性），下游 GPU 管线会把 gamma 值当
//!   线性光处理，整帧静默错色（过饱和/过亮）。HDR 桌面 float16 预期
//!   scRGB 线性（1.0=80nits），语义对拍验证见 `wgc_vs_dda_hdr_probe`。
//! - **事件驱动**：CreateFreeThreaded 自由线程帧池，FrameArrived 回调仅
//!   计数 + condvar 唤醒（回调在系统线程池，禁碰 D3D/帧对象），采集
//!   线程超时阻塞等待而非空转轮询 TryGetNextFrame。
//! - **线程模型**：所有方法须在同一线程调用，且该线程须先
//!   `CoInitializeEx(MTA)`（幂等助手 [`ensure_mta`]；recorder 集成时在
//!   capture_loop 线程入口调用）。参考 capture/ocr.rs 先例。
//! - **帧生命周期**：`acquire_frame_timeout` 取出持帧 → 调用方 GPU 拷贝
//!   → `release_frame` 归还池缓冲（池深 2，持帧不还池会耗尽停帧）。
//! - **设备归属**：模块自建 D3D11 硬件设备，经 `device()`/`context()`
//!   暴露给 record GPU 预处理管线（同设备才能 CopyResource 帧纹理）。
//!   两条实测硬约束（违反则建池/会话"看似成功但永不交付帧"）：
//!   1. 设备必须建在**显示器所在适配器**上（`factory.EnumAdapters1(
//!      monitor.adapter_index)` + `D3D_DRIVER_TYPE_UNKNOWN`；混合 GPU 机器
//!      上 None 默认适配器可能不是显示器那个）；
//!   2. 帧池尺寸必须用 **item.Size()**（物理像素，不受进程 DPI 虚拟化
//!      影响；DPI 未感知进程的 monitor.width/height 是逻辑分辨率）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use windows::core::Interface;
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FORMAT_SUPPORT_TEXTURE2D, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, IDXGIAdapter,
    IDXGIAdapter1, IDXGIFactory1, IDXGIDevice,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use super::monitor::{HdrMode, MonitorInfo};
use crate::color::PixelFormat;

/// WGC 帧的元信息（对齐 DDA 的 DXGI_OUTDUPL_FRAME_INFO 角色）
pub struct WgcFrameInfo {
    /// 呈现时刻（QPC 域，100ns 单位；相邻帧差值 / 10000 = 帧间隔 ms）
    pub system_relative_time: i64,
    /// 内容尺寸（= (monitor.width, height)，不符的帧已在 acquire 内丢弃）
    pub content_size: (i32, i32),
}

/// WGC 帧捕获器（Direct3D11CaptureFramePool 封装）
///
/// 生命周期：`for_monitor` 建池 → 循环 `acquire_frame_timeout` /
/// `release_frame` → `Closed`/失效后 `recreate`（**设备复用**）→ drop 自动清理。
pub struct WgcCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// 当前持有帧（TryGetNextFrame 取出；release_frame 置 None 归还池缓冲）
    frame: Option<Direct3D11CaptureFrame>,
    monitor: MonitorInfo,
    /// 池纹理格式（R16G16B16A16_FLOAT 或 B8G8R8A8_UNORM）
    format: DXGI_FORMAT,
    /// 期望 ContentSize（= 建池时 item.Size()，物理像素；DPI 虚拟化进程的
    /// monitor.width/height 是逻辑分辨率，不可用于比对）
    expected_content: (i32, i32),
    /// FrameArrived 回调计数 + 唤醒（见模块文档"事件驱动"）
    pending: Arc<(Mutex<u32>, Condvar)>,
    arrived_token: Option<EventRegistrationToken>,
    /// item.Closed 置位（显示器模式切换/移除 → 采集失效，需 recreate）
    closed: Arc<AtomicBool>,
    closed_token: Option<EventRegistrationToken>,
}

/// 幂等初始化当前线程为 MTA（WGC WinRT 对象的线程前提）
///
/// RPC_E_CHANGED_MODE（线程已按其它公寓模型初始化）视为成功——已有公寓
/// 即可激活 WinRT 工厂。不配对 CoUninitialize：捕获线程生命周期内保持
/// 初始化状态，避免 WinRT 回调对象被过早拆毁。
pub fn ensure_mta() -> windows::core::Result<()> {
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
        Ok(())
    } else {
        Err(windows::core::Error::from_hresult(hr))
    }
}

impl WgcCapturer {
    /// 为指定显示器创建 WGC 捕获器
    ///
    /// 格式由 `monitor.hdr_mode` 决定（HdrScRgb/Hdr10 → float16 scRGB，
    /// Sdr → BGRA8；理由见模块文档）。调用方线程须先 [`ensure_mta`]。
    pub fn for_monitor(monitor: &MonitorInfo) -> windows::core::Result<Self> {
        let format = match monitor.hdr_mode {
            HdrMode::HdrScRgb | HdrMode::Hdr10 => DXGI_FORMAT_R16G16B16A16_FLOAT,
            HdrMode::Sdr => DXGI_FORMAT_B8G8R8A8_UNORM,
        };
        Self::for_monitor_with_format(monitor, format)
    }

    /// 指定格式的构造（for_monitor 的内部泛化；测试用它建 Bgra8 池做
    /// HDR 桌面上的 f16/bgra8 同机语义对拍）
    fn for_monitor_with_format(
        monitor: &MonitorInfo,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Self> {
        // D3D11 硬件设备，**建在显示器所在适配器上**（WGC 要求帧池设备与
        // 捕获目标同适配器，否则会话看似启动但永不交付帧——混合 GPU 机器
        // 上默认适配器常不对；DDA 经 dxgi_device.GetParent 走的就是显示器
        // 自己的适配器）。record GPU 管线将建于同设备。
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
        let adapter1: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(monitor.adapter_index)? };
        let adapter: IDXGIAdapter = adapter1.cast()?;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL_11_0;
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        unsafe {
            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN, // 指定适配器时必须 UNKNOWN（D3D11 规则）
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                7, // D3D11_SDK_VERSION
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(|| {
            windows::core::Error::from(windows::core::HRESULT(0x80004005u32 as i32))
        })?;
        let context = context.ok_or_else(|| {
            windows::core::Error::from(windows::core::HRESULT(0x80004005u32 as i32))
        })?;

        // float16 池前置能力检查（HDR 路径；BGRA8 由 BGRA_SUPPORT 标志保证）
        if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            let support = unsafe { device.CheckFormatSupport(format)? };
            if support & D3D11_FORMAT_SUPPORT_TEXTURE2D.0 as u32 == 0 {
                log::error!("[wgc] D3D11 设备不支持 R16G16B16A16_FLOAT Texture2D");
                return Err(windows::core::Error::from(
                    windows::core::HRESULT(0x80004005u32 as i32),
                ));
            }
        }

        let pending = Arc::new((Mutex::new(0u32), Condvar::new()));
        let closed = Arc::new(AtomicBool::new(false));

        let (item, pool, session, expected_content, arrived_token, closed_token) =
            Self::build_pipeline(&device, monitor, format, &pending, &closed)?;

        Ok(Self {
            device,
            context,
            item,
            pool,
            session,
            frame: None,
            monitor: monitor.clone(),
            format,
            expected_content,
            pending,
            arrived_token: Some(arrived_token),
            closed,
            closed_token: Some(closed_token),
        })
    }

    /// 用指定 D3D11 设备构建 item → 帧池 → 会话并挂钩事件
    ///
    /// `for_monitor`（新设备）与 `recreate`（复用设备）共用；全部新对象先在
    /// 局部构建，失败时自动释放，不影响调用方已持有的旧状态。
    fn build_pipeline(
        device: &ID3D11Device,
        monitor: &MonitorInfo,
        format: DXGI_FORMAT,
        pending: &Arc<(Mutex<u32>, Condvar)>,
        closed: &Arc<AtomicBool>,
    ) -> windows::core::Result<(
        GraphicsCaptureItem,
        Direct3D11CaptureFramePool,
        GraphicsCaptureSession,
        (i32, i32),
        EventRegistrationToken,
        EventRegistrationToken,
    )> {
        // IDirect3DDevice（WinRT 侧设备句柄）：D3D11 设备经 DXGI 包装获得
        let dxgi_device: IDXGIDevice = device.cast()?;
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? };
        let d3d_device: IDirect3DDevice = inspectable.cast()?;

        // GraphicsCaptureItem：运行时类工厂的 COM interop 接口按 HMONITOR 创建
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(HMONITOR(monitor.hmonitor as _))? };
        // item.Size() = 物理像素（不受进程 DPI 虚拟化影响；monitor.width/height
        // 在 DPI 未感知进程里是被缩放的逻辑分辨率——池尺寸与 ContentSize 比对
        // 都必须用 item.Size()，且池与 item 同尺寸避免 DWM 缩放环节）
        let item_size = item.Size()?;

        // 自由线程帧池（不依赖 DispatcherQueue，回调在系统线程池）
        let pixel_format = match format {
            DXGI_FORMAT_R16G16B16A16_FLOAT => DirectXPixelFormat::R16G16B16A16Float,
            _ => DirectXPixelFormat::B8G8R8A8UIntNormalized,
        };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d_device,
            pixel_format,
            4, // 四缓冲：消费端抖动（GPU 预处理 10-15ms + 限流）下降低
               // 高帧率源的池溢出丢帧（实测 60fps 源 + 2 缓冲窗口过窄）
            item_size,
        )?;

        // FrameArrived：仅计数 + 唤醒（回调线程禁碰 D3D/帧对象）
        let pending_cb = pending.clone();
        let arrived = TypedEventHandler::<
            Direct3D11CaptureFramePool,
            windows::core::IInspectable,
        >::new(move |_, _| {
            let (lock, cv) = &*pending_cb;
            let mut n = lock.lock().unwrap();
            *n = n.saturating_add(1);
            cv.notify_all();
            Ok(())
        });
        let arrived_token = pool.FrameArrived(&arrived)?;

        // Closed：显示器模式切换/移除时触发（采集失效，需 recreate）
        let closed_cb = closed.clone();
        let closed_handler =
            TypedEventHandler::<GraphicsCaptureItem, windows::core::IInspectable>::new(
                move |_, _| {
                    closed_cb.store(true, Ordering::Release);
                    Ok(())
                },
            );
        let closed_token = item.Closed(&closed_handler)?;

        // 会话：隐藏光标 + 免黄边（尽力而为，旧系统失败仅告警），启动采集
        let session = pool.CreateCaptureSession(&item)?;
        if let Err(e) = session.SetIsCursorCaptureEnabled(false) {
            log::warn!("[wgc] SetIsCursorCaptureEnabled(false) 失败（旧系统?）：{}", e);
        }
        if let Err(e) = session.SetIsBorderRequired(false) {
            log::warn!("[wgc] SetIsBorderRequired(false) 失败（需 Win10 2104+）：{}", e);
        }
        session.StartCapture()?;

        Ok((
            item,
            pool,
            session,
            (item_size.Width, item_size.Height),
            arrived_token,
            closed_token,
        ))
    }

    /// 录制用：超时可配的帧获取（事件驱动，不空转轮询）
    ///
    /// 错误码约定与 DDA 对齐（recorder.rs 现有判定直接复用）：
    /// - `DXGI_ERROR_WAIT_TIMEOUT`：超时无帧（桌面静止，跳过本拍即可）
    /// - `DXGI_ERROR_ACCESS_LOST`：item 已 Closed（显示器模式切换/移除），
    ///   调用 `recreate` 重建后继续
    ///
    /// ContentSize 与显示器不符（模式切换瞬间黑边帧）或表面格式与池不符的
    /// 帧直接丢弃继续等。**调用方 GPU 消费完纹理后必须 `release_frame()`**
    /// （池深 2，持帧不还池会耗尽停帧）。
    pub(crate) fn acquire_frame_timeout(
        &mut self,
        timeout_ms: u32,
    ) -> windows::core::Result<(WgcFrameInfo, ID3D11Texture2D, PixelFormat)> {
        if self.closed.load(Ordering::Acquire) {
            return Err(windows::core::Error::from_hresult(DXGI_ERROR_ACCESS_LOST));
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
        loop {
            // 1) 非阻塞取帧（回调计数可能滞后/超前，先试再说）
            if let Ok(frame) = self.pool.TryGetNextFrame() {
                self.consume_pending();
                match self.take_frame(frame)? {
                    Some(result) => return Ok(result),
                    None => continue, // 尺寸/格式不符已丢弃，立即试下一帧
                }
            }
            // 2) 无帧可取：等 FrameArrived 唤醒（或消费 stale 通知后立即重试）
            let (lock, cv) = &*self.pending;
            let mut n = lock.lock().unwrap();
            if *n == 0 {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(windows::core::Error::from_hresult(DXGI_ERROR_WAIT_TIMEOUT));
                }
                let (guard, wait_res) = cv.wait_timeout(n, deadline - now).unwrap();
                n = guard;
                if *n > 0 {
                    *n -= 1; // 消费刚到达的通知
                } else if wait_res.timed_out() {
                    return Err(windows::core::Error::from_hresult(DXGI_ERROR_WAIT_TIMEOUT));
                }
                // 提前唤醒（spurious/竞争）：回 1) 重试，deadline 未到不超时
            } else {
                *n -= 1; // 消费通知（可能是已被取走帧的 stale 回调）：重试取帧
            }
            drop(n);
        }
    }

    /// 校验并解包 WGC 帧：尺寸/格式不符 → None（帧已 drop 归还池缓冲）；
    /// 合法 → 持帧并返回 (元信息, ID3D11Texture2D, 像素格式)
    fn take_frame(
        &mut self,
        frame: Direct3D11CaptureFrame,
    ) -> windows::core::Result<Option<(WgcFrameInfo, ID3D11Texture2D, PixelFormat)>> {
        let content = frame.ContentSize()?;
        if (content.Width, content.Height) != self.expected_content {
            // 模式切换瞬间 DWM 交付旧尺寸黑边帧：丢弃（drop 归还池缓冲）等下一帧
            log::warn!(
                "[wgc] 帧尺寸 ({}, {}) 与期望 {:?} 不符，丢弃（模式切换瞬间黑边帧）",
                content.Width,
                content.Height,
                self.expected_content
            );
            return Ok(None);
        }
        // 表面 → ID3D11Texture2D：WinRT IDirect3DSurface 经 DXGI 接口访问器解出
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        if desc.Format != self.format {
            log::warn!(
                "[wgc] 表面格式 ({:?}) 与池格式 ({:?}) 不符，丢弃",
                desc.Format.0,
                self.format.0
            );
            return Ok(None);
        }
        let time = frame.SystemRelativeTime()?;
        let pixel_format = match self.format {
            DXGI_FORMAT_R16G16B16A16_FLOAT => PixelFormat::R16g16b16a16Float,
            _ => PixelFormat::Bgra8,
        };
        let info = WgcFrameInfo {
            system_relative_time: time.Duration,
            content_size: (content.Width, content.Height),
        };
        self.frame = Some(frame);
        Ok(Some((info, tex, pixel_format)))
    }

    /// 消费一个 FrameArrived 计数（TryGetNextFrame 成功时核销对应回调；
    /// 饱和减——取帧与回调的竞争窗口允许计数轻微滞后，stale 计数由
    /// acquire 的"消费通知重试"路径核销）
    fn consume_pending(&self) {
        let (lock, _) = &*self.pending;
        let mut n = lock.lock().unwrap();
        *n = n.saturating_sub(1);
    }

    /// 录制用：释放当前持帧（drop 归还池缓冲；GPU 侧拷贝完成后调用）
    pub(crate) fn release_frame(&mut self) {
        self.frame = None;
    }

    /// 录制用：D3D11 设备引用（record GPU 预处理管线建立在同设备上——
    /// WGC 帧纹理与 RecordGpu 中转纹理同设备才能 CopyResource）
    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// 录制用：immediate context 引用（record 模块经 Mutex 串行化共享访问）
    pub(crate) fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// 采集源是否已失效（item.Closed：显示器模式切换/移除等）；
    /// 失效后 acquire 返回 DXGI_ERROR_ACCESS_LOST，需 recreate 重建
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Closed/失效后重建采集管线（item 重新创建 + 池/会话重建；
    /// **设备复用**，record GPU 管线无需重建）
    pub(crate) fn recreate(&mut self) -> windows::core::Result<()> {
        // 先在局部构建全新管线：失败则旧状态原样保留（可重试），
        // 不会陷入"拆了旧的、新的没建成"的半途状态
        let (item, pool, session, expected_content, arrived_token, closed_token) =
            Self::build_pipeline(
                &self.device,
                &self.monitor,
                self.format,
                &self.pending,
                &self.closed,
            )?;

        // 构建成功：拆除旧管线（unhook → 关池；session/item drop 即停采集）
        self.frame = None;
        if let Some(t) = self.arrived_token.take() {
            let _ = self.pool.RemoveFrameArrived(t);
        }
        if let Some(t) = self.closed_token.take() {
            let _ = self.item.RemoveClosed(t);
        }
        let _ = self.pool.Close();
        self.closed.store(false, Ordering::Release);
        // 清空旧管线残留计数（新池首帧由 acquire 的 TryGetNextFrame 优先路径兜底）
        *self.pending.0.lock().unwrap() = 0;

        self.item = item;
        self.pool = pool;
        self.session = session;
        self.expected_content = expected_content;
        self.arrived_token = Some(arrived_token);
        self.closed_token = Some(closed_token);
        Ok(())
    }
}

impl Drop for WgcCapturer {
    fn drop(&mut self) {
        // 归还持帧 → unhook 事件 → 关池；session/item drop 即停止采集
        self.frame = None;
        if let Some(t) = self.arrived_token.take() {
            let _ = self.pool.RemoveFrameArrived(t);
        }
        if let Some(t) = self.closed_token.take() {
            let _ = self.item.RemoveClosed(t);
        }
        let _ = self.pool.Close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::dxgi_duplication::DesktopCapturer;
    use crate::capture::monitor::enumerate_monitors;

    /// 探针专用：在主屏角落循环 SetPixel 强制桌面内容变化
    ///
    /// WGC 仅在内容变化时交付帧；测试进程跑在无可见窗口的后台会话里，
    /// 桌面可能完全静止——需自造活动保证帧到达的确定性。
    struct DesktopActivity {
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl DesktopActivity {
        fn start() -> Self {
            use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC, SetPixel};
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let handle = std::thread::spawn(move || {
                unsafe {
                    let dc = GetDC(None);
                    let colors: [u32; 4] = [0x0000FF, 0x00FF00, 0xFF0000, 0xFFFFFF];
                    let mut i = 0u32;
                    while !s.load(Ordering::Relaxed) {
                        SetPixel(
                            dc,
                            5,
                            5,
                            windows::Win32::Foundation::COLORREF(colors[(i % 4) as usize]),
                        );
                        i += 1;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    ReleaseDC(None, dc);
                }
            });
            Self {
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for DesktopActivity {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            // 清理角落残点：失效桌面区域让 DWM 重绘壁纸覆盖
            invalidate_screen_region(0, 0, 12, 12);
        }
    }

    /// 失效屏幕区域（让 DWM 重绘桌面内容，清理 GDI 直接画在屏幕上的探针痕迹）
    fn invalidate_screen_region(x: i32, y: i32, w: i32, h: i32) {
        use windows::Win32::Foundation::{BOOL, RECT};
        use windows::Win32::Graphics::Gdi::InvalidateRect;
        use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
        unsafe {
            let hwnd = GetDesktopWindow();
            let _ = InvalidateRect(
                hwnd,
                Some(&RECT {
                    left: x,
                    top: y,
                    right: x + w,
                    bottom: y + h,
                }),
                BOOL::from(true),
            );
        }
    }

    /// 探针专用：主屏中心置顶无边框渐变窗口
    ///
    /// 为语义判别提供**非均匀内容**（均匀色块对线性/gamma 两假设都能常数
    /// 拟合，无法判别）。GDI 直写屏幕（SetPixel on GetDC(None)）实测不进
    /// 捕获内容（中心区域被常白快速重绘窗口覆盖）；真实 Win32 窗口是
    /// DWM 合成的一等公民，WGC/DDA 都必然捕获到。
    struct ProbeGradientWindow {
        hwnd: windows::Win32::Foundation::HWND,
    }

    impl ProbeGradientWindow {
        /// 在主屏中心创建 size×size（虚拟像素）置顶窗口并画列向 0..255 灰度渐变
        fn create_at_center(m: &MonitorInfo, size: i32) -> Option<Self> {
            use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
            use windows::Win32::Graphics::Gdi::{
                CreateSolidBrush, DeleteObject, FillRect, GetDC, HGDIOBJ, ReleaseDC,
            };
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, MSG, PeekMessageW,
                RegisterClassW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, PM_REMOVE,
                SW_SHOW, WNDCLASSW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
            };
            use windows::core::w;

            // DefWindowProcW 在 windows 0.58 是泛型 Rust fn，需 extern "system" 壳
            unsafe extern "system" fn probe_wndproc(
                hwnd: HWND,
                msg: u32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> LRESULT {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }

            unsafe {
                let hinstance = GetModuleHandleW(None).ok()?;
                let class_name = w!("jietu_wgc_probe_win");
                let wc = WNDCLASSW {
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(probe_wndproc),
                    hInstance: hinstance.into(),
                    lpszClassName: class_name,
                    ..Default::default()
                };
                // 返回 0 = 失败（重复注册忽略——前次探针已注册过则继续）
                let _ = RegisterClassW(&wc);

                let cx = m.left + m.width as i32 / 2;
                let cy = m.top + m.height as i32 / 2;
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST,
                    class_name,
                    w!("wgc_probe"),
                    WS_POPUP | WS_VISIBLE,
                    cx - size / 2,
                    cy - size / 2,
                    size,
                    size,
                    None,
                    None,
                    hinstance,
                    None,
                )
                .ok()?;

                let _ = ShowWindow(hwnd, SW_SHOW);
                // 等 DWM 首帧合成，再取窗口 DC 画渐变列（窗口 DC 的 GDI 写入
                // 进入 DWM 重定向面，必然被捕获）
                std::thread::sleep(std::time::Duration::from_millis(50));
                let hdc = GetDC(hwnd);
                for x in 0..size {
                    let v = (x as u32 * 255 / (size as u32 - 1)) as u32;
                    let c = v | (v << 8) | (v << 16);
                    let brush = CreateSolidBrush(COLORREF(c));
                    let rect = RECT {
                        left: x,
                        top: 0,
                        right: x + 1,
                        bottom: size,
                    };
                    FillRect(hdc, &rect, brush);
                    let _ = DeleteObject(HGDIOBJ::from(brush));
                }
                ReleaseDC(hwnd, hdc);
                // 泵一遍消息 + 再等一拍，确保渐变进入合成内容
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                Some(Self { hwnd })
            }
        }
    }

    impl Drop for ProbeGradientWindow {
        fn drop(&mut self) {
            use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    /// 主显示器（(0,0) 或第一个）
    fn primary_monitor() -> MonitorInfo {
        let monitors = enumerate_monitors().expect("enumerate_monitors 失败");
        monitors
            .iter()
            .find(|m| m.left == 0 && m.top == 0)
            .or_else(|| monitors.first())
            .expect("无显示器")
            .clone()
    }

    /// [探针] WGC 基础采集：主显示器建池取 5 帧，打印帧间隔/尺寸/格式
    ///
    /// 帧 #1 在桌面原样下获取（观测静止桌面是否交付首帧——W3 集成的
    /// 心跳语义依据）；若失败则启动 SetPixel 活动生成器保证后续帧到达。
    #[test]
    fn wgc_probe_basic() {
        ensure_mta().expect("CoInitializeEx(MTA) 失败");
        let m = primary_monitor();
        println!(
            "[wgc_probe] 主显示器 {}x{}（逻辑分辨率，测试进程 DPI 未感知会被虚拟化）hdr_mode={:?}",
            m.width, m.height, m.hdr_mode
        );
        let mut cap = WgcCapturer::for_monitor(&m).expect("WgcCapturer::for_monitor 失败");
        let expect_f16 = m.hdr_mode.is_hdr();
        println!(
            "[wgc_probe] 池格式 = {} 尺寸 = {:?}（item.Size 物理像素；hdr 桌面期望 float16: {}）",
            if cap.format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                "R16G16B16A16_FLOAT"
            } else {
                "B8G8R8A8_UNORM"
            },
            cap.expected_content,
            expect_f16
        );
        assert_eq!(
            cap.format,
            if expect_f16 {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
            "SDR 桌面必须建 BGRA8 池 / HDR 桌面必须建 float16 池"
        );

        let mut got = 0u32;
        let mut last_time: Option<i64> = None;
        let mut activity: Option<DesktopActivity> = None;
        for i in 0..5 {
            // 帧 #1 保持桌面原样（观测静止桌面是否交付首帧——W3 集成的
            // 心跳语义依据）
            match cap.acquire_frame_timeout(500) {
                Ok((info, tex, fmt)) => {
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { tex.GetDesc(&mut desc) };
                    let dt_ms = last_time
                        .map(|t| (info.system_relative_time - t) as f64 / 10_000.0);
                    println!(
                        "[wgc_probe] 帧 #{}: sys_rel={} (+{}ms) content={:?} tex={}x{} fmt={:?}",
                        i + 1,
                        info.system_relative_time,
                        dt_ms.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "-".into()),
                        info.content_size,
                        desc.Width,
                        desc.Height,
                        fmt
                    );
                    assert_eq!(
                        info.content_size, cap.expected_content,
                        "帧内容尺寸必须等于 item.Size()（物理像素）"
                    );
                    assert_eq!(
                        (desc.Width as i32, desc.Height as i32),
                        cap.expected_content,
                        "纹理尺寸必须等于池尺寸（item.Size() 物理像素；monitor.width/height \
                         在 DPI 未感知进程是被虚拟化的逻辑分辨率，不可用于比对）"
                    );
                    last_time = Some(info.system_relative_time);
                    got += 1;
                    cap.release_frame();
                }
                Err(e) => {
                    // 诊断：FrameArrived 是否触发过（pending 计数）+ 池状态
                    let pending = *cap.pending.0.lock().unwrap();
                    let raw_try = cap.pool.TryGetNextFrame();
                    let raw_try_desc = match &raw_try {
                        Ok(f) => format!(
                            "有帧(可疑：尺寸校验前?) content={:?}",
                            f.ContentSize().map(|c| (c.Width, c.Height)).unwrap_or((0, 0))
                        ),
                        Err(err) => format!("Err {} (hr={:#010x})", err, err.code().0),
                    };
                    let item_size = cap.item.Size().map(|s| (s.Width, s.Height));
                    println!(
                        "[wgc_probe] 帧 #{} 获取失败: {} (hr={:#010x})；诊断: pending={} TryGetNextFrame=[{}] item.Size={:?} adapter={} closed={}",
                        i + 1,
                        e,
                        e.code().0,
                        pending,
                        raw_try_desc,
                        item_size,
                        m.adapter_index,
                        cap.is_closed()
                    );
                    drop(raw_try); // 诊断取出的帧（若有）直接丢弃
                    // 首次失败后启动桌面活动生成器（WGC 静止桌面出完首帧
                    // 快照后不再交付——后续帧需要内容变化驱动）
                    if activity.is_none() {
                        println!("[wgc_probe] 启动 SetPixel 桌面活动生成器（静止桌面无新帧）");
                        activity = Some(DesktopActivity::start());
                    }
                }
            }
        }
        println!("[wgc_probe] 共捕获 {}/5 帧", got);
        assert!(
            got >= 2,
            "至少应捕获 2 帧（实得 {}）。若为 0：桌面可能完全静止（WGC 无变化不出帧），\
             动一下鼠标/开个动画窗口后重跑；若持续失败请检查 FrameArrived 挂钩与会话启动",
            got
        );
    }

    /// 中心区域像素统计（R/G/B 各自的 均值|最大|P99）
    struct RegionStats {
        per_ch: [(f32, f32, f32); 3],
    }

    impl RegionStats {
        fn of(px: &[[f32; 3]]) -> Self {
            let mut per_ch = [(0f32, 0f32, 0f32); 3];
            for ch in 0..3 {
                let mut v: Vec<f32> = px.iter().map(|p| p[ch]).collect();
                if v.is_empty() {
                    continue;
                }
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let n = v.len();
                let mean = v.iter().sum::<f32>() / n as f32;
                let max = v[n - 1];
                let p99 = v[(n * 99 / 100).min(n - 1)];
                per_ch[ch] = (mean, max, p99);
            }
            RegionStats { per_ch }
        }

        fn mean_all(&self) -> f32 {
            (self.per_ch[0].0 + self.per_ch[1].0 + self.per_ch[2].0) / 3.0
        }
    }

    impl std::fmt::Display for RegionStats {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "R {:.4}|{:.4}|{:.4}  G {:.4}|{:.4}|{:.4}  B {:.4}|{:.4}|{:.4}  (mean|max|p99)",
                self.per_ch[0].0,
                self.per_ch[0].1,
                self.per_ch[0].2,
                self.per_ch[1].0,
                self.per_ch[1].1,
                self.per_ch[1].2,
                self.per_ch[2].0,
                self.per_ch[2].1,
                self.per_ch[2].2
            )
        }
    }

    /// 把纹理整幅经 staging 拷回 CPU，按 grid×grid 网格采样解码为 f32 RGB
    ///
    /// 全屏网格（而非中心小块）：本机实况是全屏游戏/视频占据画面，
    /// 多样化内容分布在全屏各处（中心可能均匀）；两源同分辨率时
    /// 相同采样序号 = 相同物理位置，可逐样本配对拟合。
    /// - R16G16B16A16_FLOAT：f16 → f32（数值原样）
    /// - B8G8R8A8_UNORM：内存序 B,G,R,A → /255 归一化（数值原样，含 gamma）
    fn map_grid_pixels(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        grid: u32,
    ) -> windows::core::Result<Vec<[f32; 3]>> {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (w, h) = (desc.Width, desc.Height);

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        let staging = staging.unwrap();

        unsafe { context.CopyResource(&staging, tex) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
        let row_pitch = mapped.RowPitch as usize;
        let bpp = match desc.Format {
            DXGI_FORMAT_R16G16B16A16_FLOAT => 8usize,
            _ => 4usize,
        };
        let mut out = Vec::with_capacity((grid * grid) as usize);
        let base = mapped.pData as *const u8;
        for gy in 0..grid {
            for gx in 0..grid {
                let x = ((gx as u64 * (w as u64 - 1)) / (grid as u64 - 1)) as u32;
                let y = ((gy as u64 * (h as u64 - 1)) / (grid as u64 - 1)) as u32;
                let off = y as usize * row_pitch + x as usize * bpp;
                // staging Map 后 [base, base + row_pitch*h) 区间有效
                let px = unsafe {
                    match desc.Format {
                        DXGI_FORMAT_R16G16B16A16_FLOAT => {
                            let b = std::slice::from_raw_parts(base.add(off), 8);
                            [
                                crate::capture::hdr_pipeline::f16_to_f32([b[0], b[1]]),
                                crate::capture::hdr_pipeline::f16_to_f32([b[2], b[3]]),
                                crate::capture::hdr_pipeline::f16_to_f32([b[4], b[5]]),
                            ]
                        }
                        _ => {
                            let b = std::slice::from_raw_parts(base.add(off), 4);
                            [b[2] as f32 / 255.0, b[1] as f32 / 255.0, b[0] as f32 / 255.0]
                        }
                    }
                };
                out.push(px);
            }
        }
        unsafe { context.Unmap(&staging, 0) };
        Ok(out)
    }

    /// 测试内 DDA 基线复刻：绑定显示器适配器设备 + DuplicateOutput1 取一帧，
    /// 读回中心 64×64 像素。含逐级诊断（本机混合 GPU 笔记本屏在 dGPU 上，
    /// DDA 在测试进程实测 UNSUPPORTED——KB3019314 混合系统 dGPU 限制）：
    /// 1) 直连适配器 DuplicateOutput1（f16 列表）
    /// 2) SetProcessDPIAware 后重试（生产 app 带 DPI 感知 manifest，测试
    ///    二进制没有——排查 DPI 虚拟化影响）
    /// 3) legacy IDXGIOutput1::DuplicateOutput
    fn dda_baseline_frame(
        m: &MonitorInfo,
    ) -> windows::core::Result<(Vec<[f32; 3]>, PixelFormat, i64)> {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R10G10B10A2_UNORM;
        use windows::Win32::Graphics::Dxgi::{
            IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
            DXGI_OUTDUPL_FRAME_INFO,
        };

        // ---- 拓扑诊断：全部适配器与其输出 ----
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
        println!("[wgc_vs_dda] 适配器拓扑:");
        for idx in 0u32.. {
            let a = match unsafe { factory.EnumAdapters1(idx) } {
                Ok(a) => a,
                Err(_) => break,
            };
            let desc = unsafe { a.GetDesc1() }?;
            let name: String = desc
                .Description
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| char::from_u32(c as u32).unwrap_or('?'))
                .collect();
            let mut outs = 0u32;
            while unsafe { a.EnumOutputs(outs) }.is_ok() {
                outs += 1;
            }
            println!(
                "[wgc_vs_dda]   adapter[{}]: {}（outputs={}）",
                idx, name, outs
            );
        }

        // ---- 建设备 + duplication 的内部助手（同一适配器）----
        let try_duplicate = |label: &str,
         legacy: bool|
         -> windows::core::Result<(IDXGIOutputDuplication, ID3D11Device, ID3D11DeviceContext)> {
            let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
            let adapter1: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(m.adapter_index)? };
            let adapter: IDXGIAdapter = adapter1.cast()?;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
            unsafe {
                D3D11CreateDevice(
                    Some(&adapter),
                    D3D_DRIVER_TYPE_UNKNOWN,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&feature_levels),
                    7,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )?;
            }
            let device = device.unwrap();
            let context = context.unwrap();
            let output: IDXGIOutput = unsafe { adapter1.EnumOutputs(m.output_index)? };
            let dupl: IDXGIOutputDuplication = if legacy {
                let o1: IDXGIOutput1 = output.cast()?;
                unsafe { o1.DuplicateOutput(&device)? }
            } else {
                let formats = [
                    DXGI_FORMAT_R16G16B16A16_FLOAT,
                    DXGI_FORMAT_R10G10B10A2_UNORM,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                ];
                let o5: IDXGIOutput5 = output.cast()?;
                unsafe { o5.DuplicateOutput1(&device, 0, &formats)? }
            };
            println!("[wgc_vs_dda] {} 成功", label);
            Ok((dupl, device, context))
        };

        // 尝试 1：当前进程状态直连
        let (dupl, device, context) = match try_duplicate("DuplicateOutput1(绑定适配器)", false) {
            Ok(v) => v,
            Err(e1) => {
                println!(
                    "[wgc_vs_dda] DuplicateOutput1(绑定适配器) 失败: {} (hr={:#010x})",
                    e1,
                    e1.code().0
                );
                // 尝试 2：DPI 感知后重试（生产 app 有 manifest，测试二进制无）
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware();
                }
                match try_duplicate("DuplicateOutput1(SetProcessDPIAware 后)", false) {
                    Ok(v) => v,
                    Err(e2) => {
                        println!(
                            "[wgc_vs_dda] DPI 感知后仍失败: {} (hr={:#010x})",
                            e2,
                            e2.code().0
                        );
                        // 尝试 3：legacy DuplicateOutput
                        match try_duplicate("legacy DuplicateOutput", true) {
                            Ok(v) => v,
                            Err(e3) => {
                                println!(
                                    "[wgc_vs_dda] legacy DuplicateOutput 也失败: {} (hr={:#010x})",
                                    e3,
                                    e3.code().0
                                );
                                return Err(e3);
                            }
                        }
                    }
                }
            }
        };

        // ---- 取有呈现时间的帧（活动生成器保证 20Hz 内容变化）----
        let mut fallback: Option<(Vec<[f32; 3]>, PixelFormat, i64)> = None;
        let mut last_err: Option<windows::core::Error> = None;
        for _ in 0..10 {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match unsafe { dupl.AcquireNextFrame(500, &mut info, &mut resource) } {
                Ok(()) => {
                    let result = resource.and_then(|res| res.cast::<ID3D11Texture2D>().ok());
                    if let Some(tex) = result {
                        let mut desc = D3D11_TEXTURE2D_DESC::default();
                        unsafe { tex.GetDesc(&mut desc) };
                        let fmt = match desc.Format {
                            DXGI_FORMAT_R16G16B16A16_FLOAT => PixelFormat::R16g16b16a16Float,
                            DXGI_FORMAT_R10G10B10A2_UNORM => PixelFormat::R10g10b10a2,
                            _ => PixelFormat::Bgra8,
                        };
                        // HDR10（PQ）不在本探针的解码范围：返回空像素仅报格式
                        let px = if fmt == PixelFormat::R10g10b10a2 {
                            Vec::new()
                        } else {
                            map_grid_pixels(&device, &context, &tex, 16)?
                        };
                        println!(
                            "[wgc_vs_dda] DDA 纹理 {}x{}（16×16 全屏网格采样）",
                            desc.Width, desc.Height
                        );
                        unsafe { let _ = dupl.ReleaseFrame(); };
                        let out = (px, fmt, info.LastPresentTime);
                        if info.LastPresentTime != 0 {
                            return Ok(out);
                        }
                        fallback = Some(out);
                        continue;
                    }
                    unsafe { let _ = dupl.ReleaseFrame(); };
                }
                Err(e) => last_err = Some(e),
            }
        }
        fallback.ok_or_else(|| {
            last_err.unwrap_or_else(|| {
                windows::core::Error::from(windows::core::HRESULT(0x80004005u32 as i32))
            })
        })
    }

    /// [探针] WGC vs DDA HDR 语义对拍：同屏各取 1 帧，读回中心 64×64 像素
    ///
    /// 判定 WGC float16 帧的编码语义（HDR 管线正确性的前提）：
    /// - DDA 也是 f16：均值比 ≈ 1.0 → VERIFIED scRGB；比值呈 2.2 幂关系 →
    ///   MISMATCH gamma-encoded（WGC 交付 sRGB gamma 编码值，按线性光解释
    ///   会静默错色，集成时需纠正）
    /// - DDA 只有 Bgra8（本机混合 GPU 实况，见 dda_baseline_frame）：改用
    ///   中心渐变逐像素判别——线性假设 wgc ≈ k·srgb_eotf(bgra) vs gamma
    ///   假设 wgc ≈ k'·bgra，单一缩放常数最小二乘拟合，残差小者胜出
    /// SDR 桌面仅做格式确认（Bgra8）。对拍结论仅打印，不作为断言。
    #[test]
    fn wgc_vs_dda_hdr_probe() {
        ensure_mta().expect("CoInitializeEx(MTA) 失败");
        let m = primary_monitor();
        println!(
            "[wgc_vs_dda] 主显示器 {}x{} hdr_mode={:?} adapter={}[{}] output={}",
            m.width,
            m.height,
            m.hdr_mode,
            m.adapter_index,
            m.adapter_name,
            m.output_index
        );
        // 活动生成器：保证 WGC/DDA 都有帧（桌面静止时两者都不出帧）
        let _activity = DesktopActivity::start();
        // 中心置顶渐变窗口：为语义判别提供非均匀内容（均匀色块对
        // 线性/gamma 两假设都能常数拟合，无法判别）
        let _gradient_win = ProbeGradientWindow::create_at_center(&m, 128)
            .expect("创建渐变窗口失败（探针需要非均匀内容判别编码语义）");
        println!("[wgc_vs_dda] 已创建中心渐变窗口（置顶 128×128 虚拟像素）");

        // DDA 基线帧：先试生产路径 DesktopCapturer；失败（本机混合 GPU 测试
        // 进程默认适配器设备 ≠ 显示器适配器 → UNSUPPORTED）回退测试内复刻
        // （设备绑定显示器适配器）。DDA 先行、读回完毕再建 WGC。
        let (dpx, dfmt, _dlast_present) = match DesktopCapturer::for_monitor(&m, true) {
            Ok(mut dda) => {
                let mut dda_frame = None;
                for _ in 0..10 {
                    match dda.acquire_frame_timeout(500) {
                        Ok(v) => {
                            dda_frame = Some(v);
                            break;
                        }
                        Err(e) => println!(
                            "[wgc_vs_dda] DDA 待帧: {} (hr={:#010x})",
                            e,
                            e.code().0
                        ),
                    }
                }
                let (dinfo, dtex, dfmt) = dda_frame
                    .expect("DDA 10×500ms 无帧（桌面完全静止？对拍需要 DDA 帧作基准）");
                let px = map_grid_pixels(dda.device(), dda.context(), &dtex, 16)
                    .expect("DDA 像素读回失败");
                dda.release_frame();
                println!(
                    "[wgc_vs_dda] DDA 帧（DesktopCapturer）: LastPresentTime={} fmt={:?}",
                    dinfo.LastPresentTime, dfmt
                );
                (px, dfmt, dinfo.LastPresentTime)
            }
            Err(e) => {
                println!(
                    "[wgc_vs_dda] DesktopCapturer::for_monitor 失败: {} (hr={:#010x})，回退测试内 DDA 复刻（设备绑定显示器适配器）",
                    e,
                    e.code().0
                );
                let (px, fmt, lp) =
                    dda_baseline_frame(&m).expect("测试内 DDA 复刻失败");
                println!(
                    "[wgc_vs_dda] DDA 帧（适配器绑定复刻）: LastPresentTime={} fmt={:?}",
                    lp, fmt
                );
                (px, fmt, lp)
            }
        };

        // WGC f16 帧（主判别数据源）
        let mut wgc = WgcCapturer::for_monitor(&m).expect("WgcCapturer::for_monitor 失败");
        let (winfo, wtex, wfmt) = wgc
            .acquire_frame_timeout(2000)
            .expect("WGC 取帧失败（2000ms）");
        println!(
            "[wgc_vs_dda] WGC f16 帧: content={:?} fmt={:?} sys_rel={}",
            winfo.content_size, wfmt, winfo.system_relative_time
        );
        let wpx = map_grid_pixels(wgc.device(), wgc.context(), &wtex, 16)
            .expect("WGC f16 像素读回失败");
        wgc.release_frame();

        // WGC Bgra8 帧（HDR 桌面上的 SDR 投影基准）：与 f16 池同机制、
        // 时间最接近的 sRGB 参照——判别 f16 语义的首选配对
        let bpx = if m.hdr_mode.is_hdr() {
            let mut wgc8 = WgcCapturer::for_monitor_with_format(&m, DXGI_FORMAT_B8G8R8A8_UNORM)
                .expect("WGC Bgra8 池创建失败（HDR 桌面强制 BGRA8 池）");
            let (i8, t8, f8) = wgc8
                .acquire_frame_timeout(2000)
                .expect("WGC Bgra8 取帧失败");
            println!(
                "[wgc_vs_dda] WGC Bgra8 帧: content={:?} fmt={:?}",
                i8.content_size, f8
            );
            let px = map_grid_pixels(wgc8.device(), wgc8.context(), &t8, 16)
                .expect("WGC Bgra8 像素读回失败");
            wgc8.release_frame();
            px
        } else {
            Vec::new()
        };

        let wstats = RegionStats::of(&wpx);
        let dstats = RegionStats::of(&dpx);
        let bstats = RegionStats::of(&bpx);
        println!("[wgc_vs_dda] 全屏 16×16 网格采样统计：");
        if !dpx.is_empty() {
            println!("[wgc_vs_dda]   DDA({:?}): {}", dfmt, dstats);
        }
        if !bpx.is_empty() {
            println!("[wgc_vs_dda]   WGC Bgra8: {}", bstats);
        }
        println!("[wgc_vs_dda]   WGC f16: {}", wstats);

        // 线性 vs gamma 判别拟合：配对样本 (sRGB 编码值 r, f16 值 w)，
        // 线性假设 w ≈ k·srgb_eotf(r)（scRGB 线性，SDR 白=k×80nits）vs
        // gamma 假设 w ≈ k'·r（gamma 编码值直接装进 f16）；
        // 单一缩放常数最小二乘拟合，平均绝对残差小者胜出
        let gray = |p: &[f32; 3]| (p[0] + p[1] + p[2]) / 3.0;
        let fit_linear_vs_gamma = |pairs: &[[f32; 2]]| -> (f32, f32, f32, f32) {
            let mut sum_we = 0f32;
            let mut sum_ee = 0f32;
            let mut sum_wr = 0f32;
            let mut sum_rr = 0f32;
            for [r, v] in pairs {
                let e = crate::color::srgb_eotf(*r);
                sum_we += v * e;
                sum_ee += e * e;
                sum_wr += v * r;
                sum_rr += r * r;
            }
            let k_lin = sum_we / sum_ee.max(1e-9);
            let k_gam = sum_wr / sum_rr.max(1e-9);
            let n = pairs.len().max(1) as f32;
            let mut err_lin = 0f32;
            let mut err_gam = 0f32;
            for [r, v] in pairs {
                err_lin += (v - k_lin * crate::color::srgb_eotf(*r)).abs();
                err_gam += (v - k_gam * r).abs();
            }
            (k_lin, err_lin / n, k_gam, err_gam / n)
        };
        let report_fit = |label: &str, pairs: &[[f32; 2]]| {
            // 内容多样性检查：sRGB 参照值分布过窄则两假设不可分
            let mut r_min = f32::MAX;
            let mut r_max = f32::MIN;
            for [r, _] in pairs {
                r_min = r_min.min(*r);
                r_max = r_max.max(*r);
            }
            if r_max - r_min < 0.05 {
                println!(
                    "[wgc_vs_dda] {} 画面内容均匀（sRGB 值域 {:.3}..{:.3}），线性/gamma 不可分——需多样化画面（游戏场景/HUD/渐变窗口可见时）重跑",
                    label, r_min, r_max
                );
                return;
            }
            let (k_lin, err_lin, k_gam, err_gam) = fit_linear_vs_gamma(pairs);
            println!(
                "[wgc_vs_dda] {} 拟合：线性 k={:.3}（SDR 白≈{:.0} nits）平均残差={:.4}；gamma k'={:.3} 平均残差={:.4}",
                label,
                k_lin,
                k_lin * 80.0,
                err_lin,
                k_gam,
                err_gam
            );
            // 首行抽样（网格第 0 行的 5 个列）
            let grid = (pairs.len() as f32).sqrt() as usize;
            for col in [0usize, grid / 4, grid / 2, grid * 3 / 4, grid - 1] {
                if let Some([r, v]) = pairs.get(col) {
                    println!(
                        "[wgc_vs_dda]   样本 {:>2}: r={:.3} f16={:.4} 线性预测={:.4} gamma预测={:.4}",
                        col,
                        r,
                        v,
                        k_lin * crate::color::srgb_eotf(*r),
                        k_gam * r
                    );
                }
            }
            if err_lin < err_gam * 0.5 {
                println!(
                    "[wgc_vs_dda] {} VERIFIED scRGB: WGC float 帧 = scRGB 线性光（线性残差 {:.4} 显著小于 gamma 残差 {:.4}；SDR 内容按 sdr_white_level 缩放）——管线按线性光直接消费",
                    label, err_lin, err_gam
                );
            } else if err_gam < err_lin * 0.5 {
                println!(
                    "[wgc_vs_dda] {} MISMATCH gamma-encoded: WGC float 帧疑似 sRGB gamma 编码（gamma 残差 {:.4} 显著小于线性残差 {:.4}）——W3 集成时需在 GPU 管线前加 sRGB 解码或改请求格式",
                    label, err_gam, err_lin
                );
            } else {
                println!(
                    "[wgc_vs_dda] {} UNKNOWN: 线性残差 {:.4} 与 gamma 残差 {:.4} 相当，无法判别（两帧时刻差导致的画面变化？）",
                    label, err_lin, err_gam
                );
            }
        };

        if m.hdr_mode.is_hdr() && wfmt == PixelFormat::R16g16b16a16Float {
            // 主判别：WGC f16 vs WGC Bgra8（同机制、时间最近）
            if !bpx.is_empty() && bpx.len() == wpx.len() {
                let pairs: Vec<[f32; 2]> = bpx
                    .iter()
                    .zip(wpx.iter())
                    .map(|(b, w)| [gray(b), gray(w)])
                    .collect();
                report_fit("[主判别 WGC-Bgra8↔WGC-f16]", &pairs);
            }
            // 次级参考：DDA f16（均值比）或 DDA Bgra8（拟合；帧时刻差大仅供参考）
            if dfmt == PixelFormat::R16g16b16a16Float && dpx.len() == wpx.len() {
                let d = dstats.mean_all();
                let w = wstats.mean_all();
                if d > 0.005 {
                    let ratio = w / d;
                    if (ratio - 1.0).abs() <= 0.15 {
                        println!(
                            "[wgc_vs_dda] [参考 DDA-f16 均值比] VERIFIED scRGB: WGC/DDA = {:.3}",
                            ratio
                        );
                    } else {
                        println!(
                            "[wgc_vs_dda] [参考 DDA-f16 均值比] WGC/DDA = {:.3}（非 1.0，详见主判别拟合）",
                            ratio
                        );
                    }
                }
            } else if dfmt == PixelFormat::Bgra8 && dpx.len() == wpx.len() {
                let pairs: Vec<[f32; 2]> = dpx
                    .iter()
                    .zip(wpx.iter())
                    .map(|(d, w)| [gray(d), gray(w)])
                    .collect();
                report_fit("[参考 DDA-Bgra8↔WGC-f16]", &pairs);
            }
        } else if m.hdr_mode.is_hdr() && wfmt != PixelFormat::R16g16b16a16Float {
            panic!(
                "HDR 桌面 WGC 主池必须是 R16G16B16A16_FLOAT（实得 {:?}）",
                wfmt
            );
        } else {
            println!(
                "[wgc_vs_dda] SDR 桌面：仅格式确认（hdr={:?} wgc={:?} dda={:?}）",
                m.hdr_mode, wfmt, dfmt
            );
            assert_eq!(
                wfmt,
                PixelFormat::Bgra8,
                "SDR 桌面 WGC 必须是 Bgra8（拒 float16 池）"
            );
        }
        // _gradient_win drop 时 DestroyWindow 自清理；_activity drop 失效角落
    }
}
