//! DXGI Desktop Duplication API 捕获
//!
//! 通过 `IDXGIOutputDuplication` 捕获桌面（含 HDR 内容）。
//! 支持 `R16G16B16A16_FLOAT`(scRGB)、`R10G10B10A2`(HDR10)、`B8G8R8A8`(SDR) 三种格式。

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter1, IDXGIDevice, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
    DXGI_OUTDUPL_FRAME_INFO,
};

use super::monitor::{enumerate_monitors, MonitorInfo};
use crate::color::PixelFormat;

/// region-select 覆盖层窗口句柄（GDI 兜底捕获时需临时隐藏）
///
/// BitBlt 会把 WDA_EXCLUDEFROMCAPTURE 窗口区域**涂黑**（与 DDA 的
/// "透出后面内容"不同），全屏覆盖层若不隐藏会导致 GDI 兜底图大面积黑块。
/// lib.rs 在覆盖层设置 display affinity 时注册句柄，0 = 未注册。
pub static REGION_SELECT_HWND: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);

/// 注册 region-select 覆盖层 hwnd（传入 0 表示注销）
pub fn set_region_select_hwnd(hwnd: isize) {
    REGION_SELECT_HWND.store(hwnd, std::sync::atomic::Ordering::Relaxed);
}

/// 是否为 Win11 及以上（build ≥ 22000）
///
/// Win10 与 Win11 的桌面复制行为差异：仅鼠标更新（LastPresentTime == 0）的帧，
/// Win11 表面仍是累积的有效桌面内容；Win10 上可能全黑，不能直接采用。
fn is_windows_11_or_later() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        // 注册表读取 build 号（RtlGetVersion 在 windows 0.58 未导出；
        // GetVersionExW 受兼容性清单影响不可靠）
        let build = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
            .and_then(|k| k.get_value::<String, _>("CurrentBuildNumber"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        // 检测失败（build=0）按 Win10 保守处理（走快照兜底路径）
        build >= 22000
    })
}

/// 桌面捕获器（持有 D3D11 设备与 duplication 句柄）
pub struct DesktopCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Option：重建 duplication 时需先销毁旧实例（同一输出同时仅允许一个）
    duplication: Option<IDXGIOutputDuplication>,
    pub monitor: MonitorInfo,
    prefer_hdr: bool,
    staging: Option<ID3D11Texture2D>,
}

/// 捕获结果
#[derive(Clone)]
pub struct CapturedTexture {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub row_pitch: usize,
    /// 捕获方式（参数记录/诊断用）：false = DXGI DDA，true = GDI BitBlt 兜底
    pub via_gdi: bool,
}

impl DesktopCapturer {
    /// 为指定显示器创建捕获器
    ///
    /// - `prefer_hdr`: true 时请求 scRGB/HDR10 格式（保留 HDR 数据）；
    ///   false 时只请求 BGRA8（系统自动 HDR→SDR 转换，不过曝，与 QQ 截图一致）
    pub fn for_monitor(monitor: &MonitorInfo, prefer_hdr: bool) -> windows::core::Result<Self> {
        // D3D11CreateDevice 签名：
        // (padapter, drivertype, software, flags, pfeaturelevels, sdkversion,
        //  ppdevice, pfeaturelevel, ppimmediatecontext)
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL_11_0;

        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
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

        // 从 D3D11 设备获取 DXGI adapter/output
        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter: IDXGIAdapter1 = unsafe { dxgi_device.GetParent() }?;
        let output = unsafe { adapter.EnumOutputs(monitor.output_index) }?;
        // DuplicateOutput1 是 IDXGIOutput5 的方法，支持指定 BGRA/HDR 格式
        let output5: IDXGIOutput5 = output.cast()?;

        // DuplicateOutput1：根据需求声明支持的格式
        // - HDR 模式：请求 scRGB/HDR10，保留高动态范围数据（用于 HDR PNG/JXL/EXR）
        // - SDR 模式：只请求 BGRA8，Windows 自动将 HDR 桌面转换为 SDR，
        //   无需色调映射，结果与 QQ 截图一致，不会过曝
        let supported_formats: &[DXGI_FORMAT] = if prefer_hdr {
            &[
                DXGI_FORMAT_R16G16B16A16_FLOAT,
                DXGI_FORMAT_R10G10B10A2_UNORM,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ]
        } else {
            &[DXGI_FORMAT_B8G8R8A8_UNORM]
        };

        let duplication = unsafe { output5.DuplicateOutput1(&device, 0, supported_formats) }?;

        Ok(DesktopCapturer {
            device,
            context,
            duplication: Some(duplication),
            monitor: monitor.clone(),
            prefer_hdr,
            staging: None,
        })
    }

    /// 用已有 D3D11 设备重建 duplication（兜底路径）
    ///
    /// 同一输出同时仅允许一个 duplication 实例，必须先销毁旧实例再创建。
    /// 调用方：录制线程 ACCESS_LOST 恢复（MPO 平面重排/独占全屏切换/
    /// 桌面合成器重置后 IDXGIOutputDuplication 失效，重新创建即可继续抓帧）。
    /// 设备/上下文不变 → record GPU 预处理管线无需重建。
    pub(crate) fn recreate_duplication(&mut self) -> windows::core::Result<()> {
        if let Some(d) = self.duplication.as_ref() {
            let _ = unsafe { d.ReleaseFrame() };
        }
        // 先销毁旧实例（置 None 触发 drop），再创建新的
        self.duplication = None;

        let dxgi_device: IDXGIDevice = self.device.cast()?;
        let adapter: IDXGIAdapter1 = unsafe { dxgi_device.GetParent() }?;
        let output = unsafe { adapter.EnumOutputs(self.monitor.output_index) }?;
        let output5: IDXGIOutput5 = output.cast()?;

        let supported_formats: &[DXGI_FORMAT] = if self.prefer_hdr {
            &[
                DXGI_FORMAT_R16G16B16A16_FLOAT,
                DXGI_FORMAT_R10G10B10A2_UNORM,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ]
        } else {
            &[DXGI_FORMAT_B8G8R8A8_UNORM]
        };

        self.duplication =
            Some(unsafe { output5.DuplicateOutput1(&self.device, 0, supported_formats) }?);
        Ok(())
    }

    /// 当前 duplication 句柄（构造与重建后保证存在）
    fn dupl(&self) -> &IDXGIOutputDuplication {
        self.duplication.as_ref().expect("duplication 未初始化")
    }

    /// 为主显示器创建捕获器
    pub fn for_monitor_primary(prefer_hdr: bool) -> windows::core::Result<Self> {
        let monitors = enumerate_monitors()?;
        let primary = monitors
            .iter()
            .find(|m| m.left == 0 && m.top == 0)
            .or_else(|| monitors.first())
            .ok_or_else(|| {
                windows::core::Error::from(windows::core::HRESULT(0x80070015u32 as i32))
            })?;
        Self::for_monitor(primary, prefer_hdr)
    }

    /// 捕获一帧桌面画面
    ///
    /// `warmup_frames`: 由于 DWM 缓冲，前几帧可能是黑帧，需要忽略（仅 Win11 生效）
    ///
    /// 帧有效性策略（Win10/Win11 行为差异）：
    /// - Win11：warmup 丢弃初始黑帧后，呈现帧/鼠标帧的表面均为累积的有效桌面内容。
    ///   但部分 Win11 机器/驱动上例外：duplication 刚创建、尚无呈现帧时，
    ///   仅鼠标更新的帧表面未初始化为桌面内容（全黑）——实测拖拽选区（高频鼠标帧）
    ///   会拿到纯黑图。因此 Win11 同样做黑帧防御：黑帧不返回，继续等呈现帧，
    ///   仍黑/超时 → GDI BitBlt 兜底（直接读取合成后的桌面，永远有效）。
    /// - Win10：部分机器/驱动上只有 `LastPresentTime != 0` 的真实呈现帧才有有效内容，
    ///   初始快照帧、鼠标帧、静止桌面超时帧全黑（实测：拖拽选区时桌面静止无呈现帧
    ///   → 黑屏）。策略：初始帧黑帧检测（非黑直接用）；黑则等待呈现帧（~500ms）；
    ///   仍无有效帧 → GDI BitBlt 兜底（直接读取合成后的桌面，永远有效）。
    pub fn capture(&mut self, warmup_frames: u32) -> windows::core::Result<CapturedTexture> {
        if !is_windows_11_or_later() {
            return self.capture_win10();
        }

        // Win11：warmup 丢弃 DWM 初始黑帧（HDR 场景）；静止桌面超时可直接忽略
        for _ in 0..warmup_frames {
            if self.acquire_and_discard().is_err() {
                break;
            }
        }

        // 黑帧防御：读到纯黑帧不返回，等下一帧（最多 5 次），仍黑 → GDI 兜底
        let mut black_tries = 0u32;
        loop {
            let (frame_info, texture, format) = match self.acquire_frame() {
                Ok(f) => f,
                Err(e) => {
                    // 超时（桌面完全静止无任何帧）/访问丢失：GDI 兜底保证有图
                    log::warn!("Win11 获取帧失败（{}），回退 GDI BitBlt 捕获", e);
                    return gdi_capture_monitor(&self.monitor);
                }
            };
            // windows-rs 中 LastPresentTime 是 i64（非 LARGE_INTEGER）
            if frame_info.LastPresentTime != 0 || frame_info.LastMouseUpdateTime != 0 {
                let tex = self.read_texture(&texture, format)?;
                if !is_mostly_black(&tex) {
                    // MPO 防御：大面积黑但非全黑（UI 可见+场景黑）= 游戏 overlay
                    // 平面偶发未合成的典型症状。再等一帧（短超时）对比——
                    // 黑区显著缩小则用新帧（MPO 已恢复），否则保留原帧。
                    let ratio = black_ratio(&tex);
                    if ratio > 0.05 {
                        if let Some(better) = self.recheck_mpo_black(ratio) {
                            return Ok(better);
                        }
                    }
                    return Ok(tex);
                }
                black_tries += 1;
                if black_tries >= 5 {
                    log::warn!(
                        "Win11 连续 {} 次黑帧（鼠标帧表面未初始化?），回退 GDI BitBlt 捕获",
                        black_tries
                    );
                    return gdi_capture_monitor(&self.monitor);
                }
                continue; // read_texture 已释放帧，继续等下一帧
            }
            unsafe { self.dupl().ReleaseFrame() }?;
        }
    }

    /// MPO 黑区复查：可疑帧（黑区 5%-95%）后再取一帧对比
    ///
    /// 返回 Some(new_tex) = 新帧黑区显著缩小（MPO 恢复，用新帧）；
    /// None = 无新帧/差异不大（保留原帧）。短超时避免静止桌面长阻塞。
    fn recheck_mpo_black(&mut self, prev_ratio: f32) -> Option<CapturedTexture> {
        let (frame_info, texture, format) = match self.acquire_frame() {
            Ok(f) => f,
            Err(_) => return None, // 超时：桌面静止，原帧即稳定
        };
        if frame_info.LastPresentTime == 0 && frame_info.LastMouseUpdateTime == 0 {
            let _ = unsafe { self.dupl().ReleaseFrame() };
            return None; // 鼠标帧：非新呈现，不参考
        }
        let tex = self.read_texture(&texture, format).ok()?;
        let new_ratio = black_ratio(&tex);
        if new_ratio + 0.15 < prev_ratio {
            log::info!(
                "MPO 黑区复查：黑区 {:.0}% → {:.0}%（overlay 平面已恢复，采用新帧）",
                prev_ratio * 100.0,
                new_ratio * 100.0
            );
            Some(tex)
        } else {
            log::info!(
                "MPO 黑区复查：黑区 {:.0}% → {:.0}%（无显著变化，保留原帧）",
                prev_ratio * 100.0,
                new_ratio * 100.0
            );
            None
        }
    }

    /// Win10 捕获策略：初始帧黑帧检测 + 等待呈现帧 + GDI 兜底
    fn capture_win10(&mut self) -> windows::core::Result<CapturedTexture> {
        // 1. 先尝试初始帧（部分 Win10 机器上快照有效）
        if let Ok((_frame_info, texture, format)) = self.acquire_frame() {
            let tex = self.read_texture(&texture, format)?;
            if !is_mostly_black(&tex) {
                return Ok(tex);
            }
            // 初始帧为黑 → 释放后继续等呈现帧
            let _ = unsafe { self.dupl().ReleaseFrame() };
            log::info!("Win10 初始帧为黑帧，等待真实呈现帧");
        }

        // 2. 等待真实呈现帧（LastPresentTime != 0）且内容非黑（最多 5 次尝试；
        //    拖拽中鼠标帧快速返回、静止桌面会超时中断，不会长时间阻塞）
        let mut waited = 0u32;
        while waited < 5 {
            match self.acquire_frame() {
                Ok((frame_info, texture, format)) => {
                    if frame_info.LastPresentTime != 0 {
                        let tex = self.read_texture(&texture, format)?;
                        if !is_mostly_black(&tex) {
                            log::info!("Win10 等到有效呈现帧（第 {} 次尝试）", waited + 1);
                            return Ok(tex);
                        }
                        // 呈现帧也是黑（如仅覆盖层更新），继续等
                    } else {
                        // 鼠标帧/超时帧：黑帧概率高，丢弃
                        let _ = unsafe { self.dupl().ReleaseFrame() };
                    }
                }
                Err(_) => {
                    // 超时（桌面完全静止，无任何帧）
                    break;
                }
            }
            waited += 1;
        }

        // 3. GDI BitBlt 兜底：直接读取合成后的桌面，不受 DDA 驱动怪癖影响
        log::info!("Win10 DDA 无有效帧，回退 GDI BitBlt 捕获");
        gdi_capture_monitor(&self.monitor)
    }

    fn acquire_and_discard(&mut self) -> windows::core::Result<()> {
        let result = unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            self.dupl()
                .AcquireNextFrame(100, &mut frame_info, &mut resource)
        };
        let _ = unsafe { self.dupl().ReleaseFrame() };
        result.map(|_| ())
    }

    fn acquire_frame(
        &mut self,
    ) -> windows::core::Result<(DXGI_OUTDUPL_FRAME_INFO, ID3D11Texture2D, PixelFormat)> {
        self.acquire_frame_timeout(2000)
    }

    /// 录制用：超时可配的裸帧获取（record 模块线程 A 循环调用，短超时轮询）
    ///
    /// 与 `acquire_frame` 的区别仅超时参数：50ms 级超时下桌面静止期快速返回
    /// DXGI_ERROR_WAIT_TIMEOUT（零数据量），不长期阻塞。
    /// 注意：调用方负责在 GPU 侧消费完纹理后调用 `release_frame()`。
    pub(crate) fn acquire_frame_timeout(
        &mut self,
        timeout_ms: u32,
    ) -> windows::core::Result<(DXGI_OUTDUPL_FRAME_INFO, ID3D11Texture2D, PixelFormat)> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        unsafe {
            self.dupl()
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)?;
        }

        let resource = resource.ok_or_else(|| {
            windows::core::Error::from(windows::core::HRESULT(0x80004005u32 as i32))
        })?;
        let texture: ID3D11Texture2D = resource.cast()?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let format = match desc.Format {
            DXGI_FORMAT_R16G16B16A16_FLOAT => PixelFormat::R16g16b16a16Float,
            DXGI_FORMAT_R10G10B10A2_UNORM => PixelFormat::R10g10b10a2,
            DXGI_FORMAT_B8G8R8A8_UNORM => PixelFormat::Bgra8,
            _ => PixelFormat::Bgra8,
        };

        Ok((frame_info, texture, format))
    }

    /// 录制用：释放当前 DDA 帧（GPU 侧拷贝完成后由 record 模块调用）
    pub(crate) fn release_frame(&mut self) {
        let _ = unsafe { self.dupl().ReleaseFrame() };
    }

    /// 录制用：D3D11 设备引用（record GPU 预处理管线建立在同一设备上）
    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// 录制用：immediate context 引用（record 模块经 Mutex 串行化共享访问）
    pub(crate) fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    fn read_texture(
        &mut self,
        desktop_texture: &ID3D11Texture2D,
        format: PixelFormat,
    ) -> windows::core::Result<CapturedTexture> {
        let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { desktop_texture.GetDesc(&mut tex_desc) };

        // 创建/复用 staging 纹理
        let need_rebuild = match &self.staging {
            Some(s) => {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                unsafe { s.GetDesc(&mut d) };
                d.Width != tex_desc.Width
                    || d.Height != tex_desc.Height
                    || d.Format != tex_desc.Format
            }
            None => true,
        };

        if need_rebuild {
            let mut staging_desc = tex_desc;
            staging_desc.Usage = D3D11_USAGE_STAGING;
            staging_desc.BindFlags = 0;
            staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            staging_desc.MiscFlags = 0;

            let mut staging: Option<ID3D11Texture2D> = None;
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
            }
            self.staging = staging;
        }

        let staging = self.staging.as_ref().unwrap();

        unsafe { self.context.CopyResource(staging, desktop_texture) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }

        let row_pitch = mapped.RowPitch as usize;
        let height = tex_desc.Height;
        let width = tex_desc.Width;
        let bpp = format.bytes_per_pixel();

        // 紧密拷贝（去除 padding）
        let src_len = row_pitch * height as usize;
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, src_len) };
        let tight_pitch = width as usize * bpp;
        let mut data = vec![0u8; tight_pitch * height as usize];
        for y in 0..height as usize {
            let src_off = y * row_pitch;
            data[y * tight_pitch..(y + 1) * tight_pitch]
                .copy_from_slice(&src[src_off..src_off + tight_pitch]);
        }

        unsafe { self.context.Unmap(staging, 0) };
        unsafe { self.dupl().ReleaseFrame()? };

        Ok(CapturedTexture {
            via_gdi: false,
            width,
            height,
            format,
            data,
            row_pitch: tight_pitch,
        })
    }
}

/// 便捷函数：一次性捕获指定显示器
pub fn capture_monitor(
    monitor: &MonitorInfo,
    prefer_hdr: bool,
) -> windows::core::Result<CapturedTexture> {
    let mut cap = DesktopCapturer::for_monitor(monitor, prefer_hdr)?;
    cap.capture(2)
}

/// GDI BitBlt 捕获（Win10 DDA 黑帧兜底）
///
/// 直接从屏幕 DC 拷贝合成后的桌面图像，不依赖 DXGI Desktop Duplication，
/// 不受 DDA 驱动怪癖（初始帧/鼠标帧全黑）影响，永远返回当前桌面的有效内容。
/// 仅支持 SDR（BGRA8）；HDR 场景回退时由系统完成 HDR→SDR 转换（与 QQ 截图一致）。
fn gdi_capture_monitor(monitor: &MonitorInfo) -> windows::core::Result<CapturedTexture> {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HGDIOBJ,
        SRCCOPY,
    };

    let w = monitor.width as i32;
    let h = monitor.height as i32;

    // 临时隐藏覆盖层：BitBlt 会把 EXCLUDEFROMCAPTURE 窗口涂黑（不是透出后面内容），
    // 全屏覆盖层不隐藏的话 GDI 图会大面积黑块。截完立即恢复显示（置顶状态保留）。
    let overlay_hwnd = REGION_SELECT_HWND.load(std::sync::atomic::Ordering::Relaxed);
    let overlay_hidden = overlay_hwnd != 0
        && unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_TOPMOST, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                SWP_SHOWWINDOW,
            };
            let ok = SetWindowPos(
                windows::Win32::Foundation::HWND(overlay_hwnd as *mut _),
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_HIDEWINDOW,
            );
            if ok.is_ok() {
                // 等 DWM 完成移除（隐藏需一帧合成时间）
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            ok.is_ok()
        };

    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(windows::core::Error::from_win32());
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        let old = SelectObject(mem_dc, HGDIOBJ::from(bmp));

        // 从显示器区域拷贝（桌面 DC 坐标 = 虚拟屏幕坐标）
        BitBlt(
            mem_dc,
            0,
            0,
            w,
            h,
            screen_dc,
            monitor.left,
            monitor.top,
            SRCCOPY,
        );

        // GetDIBits：负 height = 自上而下的顶-底行序（与 DDA 读取一致）
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut data = vec![0u8; (w * h * 4) as usize];
        let got = GetDIBits(
            mem_dc,
            bmp,
            0,
            h as u32,
            Some(data.as_mut_ptr().cast()),
            &mut bi,
            DIB_RGB_COLORS,
        );
        if got != h {
            log::error!("GDI GetDIBits 失败：期望 {} 行，实得 {}", h, got);
        }

        // 清理 GDI 资源
        SelectObject(mem_dc, old);
        let _ = DeleteObject(HGDIOBJ::from(bmp));
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);

        let result = CapturedTexture {
            via_gdi: true,
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Bgra8,
            data,
            row_pitch: (w * 4) as usize,
        };
        let black = is_mostly_black(&result);
        log::info!(
            "GDI BitBlt 捕获完成: {}x{}（显示器 {},{}）{}",
            w,
            h,
            monitor.left,
            monitor.top,
            if black {
                "【警告: 内容为黑】"
            } else {
                ""
            }
        );

        // 恢复覆盖层显示（保持置顶）
        if overlay_hidden {
            use windows::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
            };
            let _ = SetWindowPos(
                windows::Win32::Foundation::HWND(overlay_hwnd as *mut _),
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }

        Ok(result)
    }
}

/// 检测捕获内容是否接近全黑（Win10 黑帧防御）
///
/// 黑帧并非严格全 0：鼠标指针会被合成进画面，因此按"非黑像素占比"判断——
/// 黑帧仅含指针等极少量亮像素（< 0.5%），真实画面远高于此比例。
/// 极暗的真实画面可能被误判为黑帧，但代价仅是多一次快照重建（结果不变），无害。
fn is_mostly_black(tex: &CapturedTexture) -> bool {
    let ratio = black_ratio(tex);
    // 非黑像素占比 < 0.5% 视为黑帧
    ratio > 0.995
}

/// 黑像素占比（0..1；采样同 is_mostly_black）。MPO 复查用：
/// 游戏 overlay 平面未合成时黑区可达 5%-95%（UI 可见+场景黑）
fn black_ratio(tex: &CapturedTexture) -> f32 {
    let bpp = tex.format.bytes_per_pixel();
    // 各格式的 RGB 字节长度（跳过 alpha 通道）
    let rgb_len = match tex.format {
        PixelFormat::Bgra8 => 3,
        PixelFormat::R16g16b16a16Float => 6,
        PixelFormat::R10g10b10a2 => 3,
    };
    let total = tex.width as usize * tex.height as usize;
    if total == 0 {
        return 1.0;
    }
    // 采样：最多约 65536 个像素
    let step = (total / 65536).max(1);
    let mut checked = 0usize;
    let mut lit = 0usize;
    let mut i = 0usize;
    while i < total {
        let off = i * bpp;
        if off + bpp <= tex.data.len() {
            checked += 1;
            if tex.data[off..off + rgb_len].iter().any(|&b| b > 4) {
                lit += 1;
            }
        }
        i += step;
    }
    if checked == 0 {
        return 1.0;
    }
    // 黑像素占比 = 1 - 亮像素占比
    1.0 - (lit as f32 / checked as f32)
}
