//! 显示器枚举与 HDR 能力检测
//!
//! 通过 DXGI 枚举所有显示器，读取 `IDXGIOutput6::GetDesc1`
//! 判断 HDR 状态、色深、色彩空间。
//!
//! 注意：windows-rs 0.58 的 `DXGI_OUTPUT_DESC1` 不含 `SDRWhiteLevel` 字段，
//! SDR 白电平需通过 WinRT `AdvancedColorInfo` 获取（本实现暂用默认 80 nits）。

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput, IDXGIOutput6, DXGI_ADAPTER_DESC1,
    DXGI_OUTPUT_DESC, DXGI_OUTPUT_DESC1,
};

// ==================== 面板原生色域（WinRT AdvancedColorInfo） ====================

/// 面板原生色域（CIE xy 色度坐标 + 分类 + 覆盖率）
///
/// 数据融合三层：实测（WinRT/DXGI）+ 厂商标称（EDID）。
/// 峰值亮度优先 EDID 标称（与其它软件"峰值亮度"口径一致），
/// 实测值保留在 `measured_max_luminance` 供对比。
#[derive(Clone, Debug, serde::Serialize)]
pub struct PanelGamut {
    /// 红原色 (x, y)
    pub r: (f32, f32),
    /// 绿原色 (x, y)
    pub g: (f32, f32),
    /// 蓝原色 (x, y)
    pub b: (f32, f32),
    /// 白点 (x, y)
    pub white: (f32, f32),
    /// 峰值亮度（nits）：EDID 厂商标称优先，实测兜底
    pub max_luminance: f32,
    /// 全屏持续亮度（nits，MaxFullFrameLuminance）
    pub max_full_frame_luminance: f32,
    /// 色域分类：BT.709 / DCI-P3 / BT.2020（按原色距离最近者）
    pub name: String,
    /// 对 DCI-P3 的覆盖率（0-1，面积比近似）
    pub p3_coverage: f32,
    /// 对 BT.2020 的覆盖率（0-1）
    pub bt2020_coverage: f32,
    /// 色域/亮度数据来源（EDID / WinRT / DXGI）
    pub source: String,
    /// 输出所在显卡（DXGI adapter 描述，如 "NVIDIA GeForce RTX 4070"）
    pub adapter_name: String,
    /// EDID 显示器名（如 "CSO Main Monitor"）
    pub monitor_name: Option<String>,
    /// EDID 厂商标称峰值（nits；驱动实测通常低于此值）
    pub edid_max_luminance: Option<f32>,
    /// 驱动/系统实测峰值（nits，DXGI MaxLuminance）
    pub measured_max_luminance: f32,
}

/// 面板色域缓存（TTL 30s：面板物理属性不变，HDR 开关/模式切换会刷新）
static PANEL_GAMUT_CACHE: std::sync::Mutex<Option<(PanelGamut, u64)>> = std::sync::Mutex::new(None);

/// 读取当前面板原生色域（自动识别，三通道融合）
///
/// 1. **WinRT AdvancedColorInfo**（首选）：`GetAdvancedColorInfo` 给出面板实测
///    R/G/B/白点 xy。注意 `GetForCurrentView` 需要 CoreWindow，桌面应用
///    （Tauri 无 UWP 视图）常常失败 —— 失败自动落到 2。
/// 2. **DXGI GetDesc1**（兜底）：枚举显示器时已读的面板原色。HDR 已开启的
///    显示器必报原生面板色域（驱动按物理面板报告）；全 SDR 时取主显示器
///    （部分驱动此时只报 709 信号色域，卡片上注明即可）。
/// 3. **EDID 标称融合**：厂商 EDID 的标称峰值（CTA-861 HDR 元数据）与
///    标称原色（基础块色度坐标）覆盖实测值——峰值优先 EDID（其它软件口径）；
///    原色仅在 EDID 面积明显更大时采用（部分厂商基础块只填 sRGB 占位值）。
///
/// 面板色域随显示器物理属性固定，30s 缓存足够（热插拔/远程会话场景刷新）。
pub fn get_panel_gamut() -> Option<PanelGamut> {
    const TTL_SECS: u64 = 30;
    {
        let cached = PANEL_GAMUT_CACHE.lock().ok()?;
        if let Some((g, t)) = cached.as_ref() {
            if now_secs().saturating_sub(*t) < TTL_SECS {
                return Some(g.clone());
            }
        }
    }

    // pick 显示器（HDR 优先 → 主显示器 → 第一个），供显卡名 + EDID 融合
    let monitors = enumerate_monitors().ok().unwrap_or_default();
    let pick = pick_monitor(&monitors);

    let mut fresh = panel_gamut_winrt().or_else(panel_gamut_dxgi);
    // WinRT/DXGI 全失败（SDR 模式且驱动未报原色）→ 纯 EDID 标称构建
    if fresh.is_none() {
        if let Some(m) = &pick {
            if let Some(e) = &m.edid {
                if let Some((r, g, b, w)) = e.valid_primaries() {
                    fresh = build_panel_gamut(
                        r,
                        g,
                        b,
                        w,
                        e.desired_max_luminance.unwrap_or(0.0),
                        m.max_full_frame_luminance,
                        "EDID",
                    );
                }
            }
        }
    }
    if let Some(g) = fresh.as_mut() {
        merge_edid(g, pick.as_ref());
    }

    if let Some(g) = &fresh {
        if let Ok(mut c) = PANEL_GAMUT_CACHE.lock() {
            *c = Some((g.clone(), now_secs()));
        }
    }
    fresh
}

/// 显示器选择：HDR 已开启优先，其次主显示器（left=0,top=0），最后第一个
fn pick_monitor(monitors: &[MonitorInfo]) -> Option<MonitorInfo> {
    monitors
        .iter()
        .find(|m| m.is_hdr() && m.gamut_valid)
        .or_else(|| monitors.iter().find(|m| m.left == 0 && m.top == 0))
        .or_else(|| monitors.first())
        .cloned()
}

/// EDID 标称值融合进实测 PanelGamut（原地）
///
/// - `adapter_name`：输出所在显卡（DXGI 枚举）
/// - 峰值：EDID 标称 > 0 时覆盖 `max_luminance`，实测保留到 `measured_max_luminance`
/// - 原色：EDID 色域面积比实测大 5% 以上才采用（排除厂商 sRGB 占位值），
///   采用后重算分类与覆盖率
fn merge_edid(g: &mut PanelGamut, pick: Option<&MonitorInfo>) {
    let Some(m) = pick else { return };
    g.adapter_name = m.adapter_name.clone();
    let Some(e) = &m.edid else {
        log::info!("[面板色域] EDID 不可用（{}），仅实测数据", m.device_name);
        return;
    };
    g.monitor_name.clone_from(&e.monitor_name);

    // 峰值：EDID 标称优先（其它软件"峰值亮度"口径），实测兜底
    if let Some(nom) = e.desired_max_luminance {
        if nom > 0.0 {
            g.edid_max_luminance = Some(nom);
            if g.measured_max_luminance <= 0.0 {
                g.measured_max_luminance = g.max_luminance;
            }
            g.max_luminance = nom;
        }
    }

    // 原色：EDID 面积明显更大 → 标称色域覆盖实测
    if let Some((er, eg, eb, ew)) = e.valid_primaries() {
        let edid_area = tri_area(er, eg, eb);
        let meas_area = tri_area(g.r, g.g, g.b);
        if edid_area > meas_area * 1.05 {
            log::info!(
                "[面板色域] EDID 标称色域面积 {:.4} > 实测 {:.4} → 采用 EDID 原色",
                edid_area,
                meas_area
            );
            g.r = er;
            g.g = eg;
            g.b = eb;
            g.white = ew;
            g.name = classify_gamut(er, eg, eb).to_string();
            g.p3_coverage = coverage_vs(er, eg, eb, P3_PRIMS);
            g.bt2020_coverage = coverage_vs(er, eg, eb, BT2020_PRIMS);
            g.source = "EDID".to_string();
        }
    }
}

/// WinRT 通道（独立 STA 线程；桌面应用可能失败，返回 None 走 DXGI）
fn panel_gamut_winrt() -> Option<PanelGamut> {
    std::thread::spawn(|| unsafe {
        use windows::Graphics::Display::DisplayInformation;
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };
        if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
            return None;
        }
        let result = (|| {
            let info = match DisplayInformation::GetForCurrentView() {
                Ok(i) => i,
                Err(e) => {
                    log::info!("[面板色域] WinRT GetForCurrentView 不可用（桌面应用无 CoreView，走 DXGI）: {}", e);
                    return None;
                }
            };
            let ac = info.GetAdvancedColorInfo().ok()?;
            let rd = ac.RedPrimary().ok()?;
            let gn = ac.GreenPrimary().ok()?;
            let bl = ac.BluePrimary().ok()?;
            let wh = ac.WhitePoint().ok()?;
            // WinRT MaxLuminanceInNits = 小面积峰值；全屏亮度从 DXGI 枚举取
            // （WinRT 无对应字段；失败兜底 = 峰值 × 0.3 经验比）
            let max_lum = ac.MaxLuminanceInNits().unwrap_or(0.0);
            let full_frame = enumerate_monitors()
                .ok()
                .and_then(|ms| {
                    ms.iter()
                        .find(|m| m.is_hdr())
                        .map(|m| m.max_full_frame_luminance)
                })
                .filter(|v| *v > 0.0)
                .unwrap_or(max_lum * 0.3);
            build_panel_gamut(
                (rd.X, rd.Y),
                (gn.X, gn.Y),
                (bl.X, bl.Y),
                (wh.X, wh.Y),
                max_lum,
                full_frame,
                "WinRT",
            )
        })();
        let _ = CoUninitialize();
        result
    })
    .join()
    .ok()
    .flatten()
}

/// DXGI 通道（枚举缓存数据；HDR 显示器优先，其次主显示器/第一个）
fn panel_gamut_dxgi() -> Option<PanelGamut> {
    let monitors = enumerate_monitors().ok()?;
    // HDR 已开启的显示器：驱动按物理面板报告原色（最可靠）
    let pick = monitors
        .iter()
        .find(|m| m.is_hdr() && m.gamut_valid)
        .or_else(|| {
            // 全 SDR：主显示器（left=0,top=0 或第一个附桌面输出）
            monitors
                .iter()
                .find(|m| m.gamut_valid && m.left == 0 && m.top == 0)
                .or_else(|| monitors.iter().find(|m| m.gamut_valid))
        })?;
    build_panel_gamut(
        pick.gamut_r,
        pick.gamut_g,
        pick.gamut_b,
        pick.gamut_white,
        pick.max_luminance,
        pick.max_full_frame_luminance,
        "DXGI",
    )
}

/// 原色 → 分类 + 覆盖率 + PanelGamut（双通道共用）
fn build_panel_gamut(
    r: (f32, f32),
    g: (f32, f32),
    b: (f32, f32),
    white: (f32, f32),
    max_luminance: f32,
    max_full_frame_luminance: f32,
    source: &str,
) -> Option<PanelGamut> {
    // 原色全零/明显非法（xy 每分量应在 0-1）= 驱动未报告
    if r.0 <= 0.0 || g.0 <= 0.0 || b.0 <= 0.0 || white.0 <= 0.0 {
        return None;
    }
    let name = classify_gamut(r, g, b).to_string();
    let p3_cov = coverage_vs(r, g, b, P3_PRIMS);
    let bt2020_cov = coverage_vs(r, g, b, BT2020_PRIMS);
    log::info!(
        "[面板色域/{}] {} 原色 R({:.3},{:.3}) G({:.3},{:.3}) B({:.3},{:.3}) 白点({:.3},{:.3}) 峰值 {:.0}nits 全屏 {:.0}nits P3覆盖{:.0}% 2020覆盖{:.0}%",
        source, name, r.0, r.1, g.0, g.1, b.0, b.1, white.0, white.1,
        max_luminance, max_full_frame_luminance, p3_cov * 100.0, bt2020_cov * 100.0
    );
    Some(PanelGamut {
        r,
        g,
        b,
        white,
        max_luminance,
        max_full_frame_luminance,
        name,
        p3_coverage: p3_cov,
        bt2020_coverage: bt2020_cov,
        source: source.to_string(),
        adapter_name: String::new(),
        monitor_name: None,
        edid_max_luminance: None,
        measured_max_luminance: max_luminance,
    })
}

/// 前端命令：读取面板色域（TitleBar 显示器面板 / HDR 徽标展示用）
#[tauri::command]
pub fn panel_gamut() -> Option<PanelGamut> {
    get_panel_gamut()
}

/// 已知色域原色（x, y）：709 / P3 / 2020
const BT709_PRIMS: [(f32, f32); 3] = [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)];
const P3_PRIMS: [(f32, f32); 3] = [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)];
const BT2020_PRIMS: [(f32, f32); 3] = [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)];

/// 按原色总距离分类（最近者命名）
fn classify_gamut(r: (f32, f32), g: (f32, f32), b: (f32, f32)) -> &'static str {
    let dist = |prims: [(f32, f32); 3]| -> f32 {
        let (dr, dg, db) = (prims[0], prims[1], prims[2]);
        ((r.0 - dr.0).powi(2)
            + (r.1 - dr.1).powi(2)
            + (g.0 - dg.0).powi(2)
            + (g.1 - dg.1).powi(2)
            + (b.0 - db.0).powi(2)
            + (b.1 - db.1).powi(2))
        .sqrt()
    };
    let d709 = dist(BT709_PRIMS);
    let dp3 = dist(P3_PRIMS);
    let d2020 = dist(BT2020_PRIMS);
    if d709 <= dp3 && d709 <= d2020 {
        "BT.709"
    } else if dp3 <= d2020 {
        "DCI-P3"
    } else {
        "BT.2020"
    }
}

/// 三角形面积（xy 色度图上）
fn tri_area(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
    ((b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1)).abs() * 0.5
}

/// 覆盖率（面板三角形面积 / 参考色域面积，截到 1.0；交集近似）
fn coverage_vs(r: (f32, f32), g: (f32, f32), b: (f32, f32), ref_prims: [(f32, f32); 3]) -> f32 {
    let panel = tri_area(r, g, b);
    let reference = tri_area(ref_prims[0], ref_prims[1], ref_prims[2]);
    if reference <= f32::EPSILON {
        return 0.0;
    }
    (panel / reference).min(1.0)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 色彩空间分类
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrMode {
    /// SDR 显示器（B8G8R8A8 / 8bit）
    Sdr,
    /// Windows HDR 已开启（scRGB / FP16）
    HdrScRgb,
    /// HDR10 输出模式（PQ / R10G10B10A2）
    Hdr10,
}

impl HdrMode {
    pub fn is_hdr(self) -> bool {
        !matches!(self, HdrMode::Sdr)
    }
}

/// 单个显示器的描述信息
#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub adapter_index: u32,
    pub output_index: u32,
    pub adapter_name: String,
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub bits_per_color: u32,
    pub hdr_mode: HdrMode,
    /// SDR 白电平（nits），windows-rs 不提供，使用默认 80
    pub sdr_white_level_nits: f32,
    /// 显示器最大全帧亮度（nits），来自 MaxFullFrameLuminance
    pub max_full_frame_luminance: f32,
    /// 显示器小面积峰值亮度（nits），来自 DXGI MaxLuminance（HDR 高光实际能力）
    pub max_luminance: f32,
    pub attached_to_desktop: bool,
    pub hmonitor: isize,
    /// 面板红原色 xy（DXGI_OUTPUT_DESC1；HDR 开启时为面板原生色域）
    pub gamut_r: (f32, f32),
    /// 面板绿原色 xy
    pub gamut_g: (f32, f32),
    /// 面板蓝原色 xy
    pub gamut_b: (f32, f32),
    /// 白点 xy
    pub gamut_white: (f32, f32),
    /// 是否取到有效面板色域（输出6缺失/驱动全零时 false）
    pub gamut_valid: bool,
    /// EDID 厂商标称数据（峰值亮度/色域原色/显示器名；注册表读取）
    pub edid: Option<crate::capture::edid::EdidInfo>,
}

impl MonitorInfo {
    pub fn rect(&self) -> RECT {
        RECT {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }

    pub fn is_hdr(&self) -> bool {
        self.hdr_mode.is_hdr()
    }
}

/// 枚举系统中所有已连接到桌面的显示器
pub fn enumerate_monitors() -> windows::core::Result<Vec<MonitorInfo>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut result = Vec::new();

    let mut adapter_index = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(_) => break,
        };

        let adapter_desc: DXGI_ADAPTER_DESC1 = unsafe { adapter.GetDesc1() }?;
        let adapter_name = utf16_to_string(&adapter_desc.Description);

        let mut output_index = 0u32;
        loop {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(_) => break,
            };

            if let Some(info) = probe_output(&output, adapter_index, output_index, &adapter_name)? {
                if info.attached_to_desktop {
                    result.push(info);
                }
            }

            output_index += 1;
        }
        adapter_index += 1;
    }

    Ok(result)
}

fn probe_output(
    output: &IDXGIOutput,
    adapter_index: u32,
    output_index: u32,
    adapter_name: &str,
) -> windows::core::Result<Option<MonitorInfo>> {
    let desc: DXGI_OUTPUT_DESC = unsafe { output.GetDesc() }?;
    let device_name = utf16_to_string(&desc.DeviceName);
    let attached = desc.AttachedToDesktop.as_bool();
    let rect = desc.DesktopCoordinates;
    // EDID 厂商标称数据（仅桌面输出；注册表读取）
    let edid = if attached {
        crate::capture::edid::read_edid(&device_name)
    } else {
        None
    };

    // 尝试获取 IDXGIOutput6（HDR 信息）
    let output6: IDXGIOutput6 = match output.cast::<IDXGIOutput6>() {
        Ok(o) => o,
        Err(_) => {
            return Ok(Some(MonitorInfo {
                adapter_index,
                output_index,
                adapter_name: adapter_name.to_string(),
                device_name,
                width: (rect.right - rect.left) as u32,
                height: (rect.bottom - rect.top) as u32,
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
                bits_per_color: 8,
                hdr_mode: HdrMode::Sdr,
                sdr_white_level_nits: 80.0,
                max_full_frame_luminance: 80.0,
                max_luminance: 80.0,
                attached_to_desktop: attached,
                hmonitor: desc.Monitor.0 as isize,
                gamut_r: (0.0, 0.0),
                gamut_g: (0.0, 0.0),
                gamut_b: (0.0, 0.0),
                gamut_white: (0.0, 0.0),
                gamut_valid: false,
                edid,
            }));
        }
    };

    let desc1: DXGI_OUTPUT_DESC1 = unsafe { output6.GetDesc1() }?;

    let hdr_mode = if desc1.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
        && desc1.BitsPerColor >= 10
    {
        HdrMode::HdrScRgb
    } else {
        HdrMode::Sdr
    };

    // 面板原色（xy 色度坐标；全零 = 驱动未报告 → 视为无效）
    let (gr, gg, gb, gw) = (
        (desc1.RedPrimary[0], desc1.RedPrimary[1]),
        (desc1.GreenPrimary[0], desc1.GreenPrimary[1]),
        (desc1.BluePrimary[0], desc1.BluePrimary[1]),
        (desc1.WhitePoint[0], desc1.WhitePoint[1]),
    );
    let gamut_valid =
        gr.0 != 0.0 || gr.1 != 0.0 || gg.0 != 0.0 || gg.1 != 0.0 || gb.0 != 0.0 || gb.1 != 0.0;

    Ok(Some(MonitorInfo {
        adapter_index,
        output_index,
        adapter_name: adapter_name.to_string(),
        device_name,
        width: (rect.right - rect.left) as u32,
        height: (rect.bottom - rect.top) as u32,
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
        bits_per_color: desc1.BitsPerColor,
        hdr_mode,
        // windows-rs 不暴露 SDRWhiteLevel，用默认 80 nits
        sdr_white_level_nits: 80.0,
        max_full_frame_luminance: desc1.MaxFullFrameLuminance,
        max_luminance: desc1.MaxLuminance,
        attached_to_desktop: attached,
        hmonitor: desc.Monitor.0 as isize,
        gamut_r: gr,
        gamut_g: gg,
        gamut_b: gb,
        gamut_white: gw,
        gamut_valid,
        edid,
    }))
}

fn utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// 读取系统 SDR 白电平（nits），用于 HDR 色调映射的归一化基准。
///
/// 使用 Win32 `DisplayConfigGetDeviceInfo` + `DISPLAYCONFIG_SDR_WHITE_LEVEL`
/// 官方 API 读取。这是 Windows HDR 设置中"SDR 内容亮度"滑块对应的值。
///
/// 换算公式（来自微软文档）：
///   SDRWhiteLevelInNits = SDRWhiteLevel / 1000 * 80
///
/// 失败时返回默认 80 nits。
pub fn get_sdr_white_level_nits() -> f32 {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL, QDC_ALL_PATHS,
    };

    // 1. 获取缓冲区大小
    let mut num_paths: u32 = 0;
    let mut num_modes: u32 = 0;
    let hr = unsafe { GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut num_paths, &mut num_modes) };
    if hr.is_err() {
        log::warn!("GetDisplayConfigBufferSizes 失败: {:?}", hr);
        return 80.0;
    }

    // 2. 查询所有显示路径
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
        vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
        vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
    let hr = unsafe {
        QueryDisplayConfig(
            QDC_ALL_PATHS,
            &mut num_paths,
            paths.as_mut_ptr(),
            &mut num_modes,
            modes.as_mut_ptr(),
            None,
        )
    };
    if hr.is_err() {
        log::warn!("QueryDisplayConfig 失败: {:?}", hr);
        return 80.0;
    }
    paths.truncate(num_paths as usize);

    // 3. 对每个 path 查询 SDR 白电平，取第一个有效值
    for path in &paths {
        let mut sdr_info: DISPLAYCONFIG_SDR_WHITE_LEVEL = unsafe { std::mem::zeroed() };
        sdr_info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
        sdr_info.header.size = std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
        sdr_info.header.adapterId = path.targetInfo.adapterId;
        sdr_info.header.id = path.targetInfo.id;

        // DisplayConfigGetDeviceInfo 返回 i32（Win32 错误码），0 = 成功
        let hr = unsafe { DisplayConfigGetDeviceInfo(&mut sdr_info.header as *mut _ as *mut _) };
        if hr != 0 {
            continue;
        }

        // 换算（微软官方公式）：SDRWhiteLevel / 1000 * 80
        let raw = sdr_info.SDRWhiteLevel;
        let nits = raw as f32 / 1000.0 * 80.0;
        if nits > 0.0 && nits < 10000.0 {
            log::info!("系统 SDR 白电平: {} nits (raw={}, 显示器目标)", nits, raw);
            return nits;
        } else {
            log::warn!("SDR 白电平异常 raw={} → {} nits，跳过", raw, nits);
        }
    }

    log::warn!("无法通过 DisplayConfig 读取 SDR 白电平，使用默认 80 nits");
    80.0
}

/// SDR 白电平记忆缓存（进程内；避免每次编码都读 config 文件）
static SDR_WHITE_MEMO: std::sync::Mutex<Option<f32>> = std::sync::Mutex::new(None);

/// SDR→HDR 编码的参考白（nits）——SDR 内容还原到 HDR 容器用的"白"亮度
///
/// - HDR 开启：DisplayConfig 滑块即真实参考白（本机 200nits）→ 直接用，并记忆到
///   config（值变化才写盘，进程内 memo 去重）
/// - SDR 模式：滑块不可读（raw 恒 1000 → 80nits），用 config 记忆的"上次 HDR 期
///   参考白"——否则 SDR 模式下放大输出的 HDR JXL（白=80nits）在之后开启的 HDR
///   屏上只有真实亮度的 40%（SDR→HDR 未还原）
pub fn effective_sdr_white_nits() -> f32 {
    let cur = get_sdr_white_level_nits();
    if cur > 80.5 {
        // HDR 开启：记忆真实参考白
        let rounded = (cur * 10.0).round() / 10.0;
        let mut memo = SDR_WHITE_MEMO.lock().unwrap_or_else(|e| e.into_inner());
        if memo.as_ref() != Some(&rounded) {
            let mut cfg = crate::config::Config::load();
            cfg.last_hdr_sdr_white = Some(rounded);
            match cfg.save() {
                Ok(()) => *memo = Some(rounded),
                Err(e) => log::warn!("SDR 参考白记忆写入失败: {}", e),
            }
        }
        return cur;
    }
    // SDR 模式：滑块不可读 → 进程 memo → config 记忆 → 80 兜底
    let memo = SDR_WHITE_MEMO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .copied();
    match memo.or_else(|| crate::config::Config::load().last_hdr_sdr_white) {
        Some(v) if v > 80.5 => v,
        _ => 80.0,
    }
}
