//! Windows 11 HDR 开关控制
//!
//! 通过 Win32 DisplayConfig API 实现：
//! - 状态查询：`DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO`
//! - 开关切换：`DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE`
//!
//! 注意：切换时屏幕会黑闪 1-2 秒（系统重新协商显示模式），
//! 部分系统上 `DisplayConfigSetDeviceInfo` 需要管理员权限。

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_DEVICE_INFO_TYPE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};

/// 未公开的 SET_SDR_WHITE_LEVEL 设备信息类型（Windows SDK 未导出，
/// 社区验证值 0xFFFFFFEE，Windows 设置面板同款路径）
const DISPLAYCONFIG_DEVICE_INFO_SET_SDR_WHITE_LEVEL: DISPLAYCONFIG_DEVICE_INFO_TYPE =
    DISPLAYCONFIG_DEVICE_INFO_TYPE(0xFFFFFFEEu32 as i32);

/// SET_SDR_WHITE_LEVEL 用的结构体（比 GET 版多 finalValue 字段）
#[repr(C)]
struct SetSdrWhiteLevel {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    sdr_white_level: u32,
    final_value: u8,
}

/// 单个显示器的 HDR 状态
#[derive(Debug, Clone, serde::Serialize)]
pub struct HdrMonitorState {
    /// GDI 设备名（如 \\.\DISPLAY1）
    pub device_name: String,
    /// 显示器友好名称（如 "DELL U2723QE"）
    pub friendly_name: String,
    /// 是否支持 HDR
    pub hdr_supported: bool,
    /// HDR 是否已开启
    pub hdr_enabled: bool,
    /// HDR 内容亮度（SDR 白电平，单位 nits；查询失败或 HDR 关闭时为 0）
    pub sdr_white_nits: f32,
}

/// 查询所有活动显示器的 HDR 状态
pub fn get_hdr_states() -> Result<Vec<HdrMonitorState>, String> {
    let paths = query_active_paths()?;

    let mut result = Vec::new();
    for path in &paths {
        let target_adapter = path.targetInfo.adapterId;
        let target_id = path.targetInfo.id;

        // 1. 查询高级颜色信息（HDR 支持与开关状态）
        let color_info = match query_advanced_color(target_adapter, target_id) {
            Some(info) => info,
            None => continue,
        };
        let value = unsafe { color_info.Anonymous.value };
        // 位域布局（Windows SDK）：
        // bit0 AdvancedColorSupported / bit1 AdvancedColorEnabled
        // bit2 WideColorSpaceSupported / bit3 HighDynamicRangeSupported
        let supported = value & 0x1 != 0;
        let enabled = value & 0x2 != 0;

        // 2. 查询源设备名（\\.\DISPLAY1）
        let device_name =
            query_source_name(path.sourceInfo.adapterId, path.sourceInfo.id).unwrap_or_default();

        // 3. 查询显示器友好名称
        let friendly_name =
            query_target_name(target_adapter, target_id).unwrap_or_else(|| "未知显示器".into());

        // 4. 查询 HDR 内容亮度（SDR 白电平，HDR 模式下生效）
        let sdr_white_nits = if enabled {
            query_sdr_white_level(target_adapter, target_id).unwrap_or(0.0)
        } else {
            0.0
        };

        result.push(HdrMonitorState {
            device_name,
            friendly_name,
            hdr_supported: supported,
            hdr_enabled: enabled,
            sdr_white_nits,
        });
    }

    Ok(result)
}

/// 切换 HDR 开关
///
/// - `device_name`: 目标显示器（如 "\\.\DISPLAY1"）；None 时应用到所有支持 HDR 的显示器
/// - `enable`: true 开启 / false 关闭
///
/// 返回实际被切换的显示器数量。
pub fn set_hdr_state(device_name: Option<&str>, enable: bool) -> Result<usize, String> {
    let paths = query_active_paths()?;
    let mut changed = 0usize;

    for path in &paths {
        let target_adapter = path.targetInfo.adapterId;
        let target_id = path.targetInfo.id;

        let color_info = match query_advanced_color(target_adapter, target_id) {
            Some(info) => info,
            None => continue,
        };
        let value = unsafe { color_info.Anonymous.value };
        let supported = value & 0x1 != 0;
        let enabled = value & 0x2 != 0;

        // 不支持 HDR 的显示器跳过
        if !supported {
            continue;
        }

        // 已处于目标状态时跳过
        if enabled == enable {
            continue;
        }

        // 指定了显示器时只处理匹配的
        if let Some(name) = device_name {
            let dn = query_source_name(path.sourceInfo.adapterId, path.sourceInfo.id)
                .unwrap_or_default();
            if dn != name {
                continue;
            }
        }

        let friendly = query_target_name(target_adapter, target_id).unwrap_or_default();
        log::info!(
            "切换 HDR: {} ({}) → {}",
            friendly,
            query_source_name(path.sourceInfo.adapterId, path.sourceInfo.id).unwrap_or_default(),
            if enable { "开启" } else { "关闭" }
        );

        // 发送 SET 请求（bit0 = EnableAdvancedColor）
        let mut set_info: DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE = unsafe { std::mem::zeroed() };
        set_info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE;
        set_info.header.size = std::mem::size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>() as u32;
        set_info.header.adapterId = target_adapter;
        set_info.header.id = target_id;
        unsafe {
            set_info.Anonymous.value = if enable { 1 } else { 0 };
        }

        let hr = unsafe { DisplayConfigSetDeviceInfo(&set_info.header as *const _ as *const _) };
        if hr != 0 {
            let msg = format!(
                "DisplayConfigSetDeviceInfo 失败 (错误码 {}): {}",
                hr,
                win32_error_message(hr)
            );
            log::error!("{}", msg);
            return Err(msg);
        }
        changed += 1;
    }

    Ok(changed)
}

// === 内部工具 ===

/// 设置指定显示器的 HDR 内容亮度（SDR 白电平）
///
/// Windows 11「HDR 内容亮度 / SDR 内容亮度」滑块的同款能力：
/// HDR 开启时控制 SDR 内容（桌面应用）的显示亮度。
/// - `device_name`: 目标显示器（如 "\\.\DISPLAY1"）
/// - `nits`: SDR 白电平（nits，有效范围约 80-480，内部按 4 对齐匹配系统滑块步进）
///
/// API 编码：`SDRWhiteLevel` = 80 nits 的倍数 × 1000（200 nits → 2500）。
pub fn set_sdr_white_level(device_name: &str, nits: f32) -> Result<(), String> {
    let paths = query_active_paths()?;
    for path in &paths {
        let dn =
            query_source_name(path.sourceInfo.adapterId, path.sourceInfo.id).unwrap_or_default();
        if dn != device_name {
            continue;
        }
        let target_adapter = path.targetInfo.adapterId;
        let target_id = path.targetInfo.id;

        let mut info: SetSdrWhiteLevel = unsafe { std::mem::zeroed() };
        info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_SET_SDR_WHITE_LEVEL;
        info.header.size = std::mem::size_of::<SetSdrWhiteLevel>() as u32;
        info.header.adapterId = target_adapter;
        info.header.id = target_id;
        // nits → 4 的倍数（系统滑块步进）→ 80 nits 倍数 × 1000
        let n4 = (nits / 4.0).round().max(1.0) * 4.0;
        info.sdr_white_level = (n4 * 1000.0 / 80.0).round() as u32;
        info.final_value = 1;

        let hr = unsafe { DisplayConfigSetDeviceInfo(&info.header as *const _ as *const _) };
        if hr != 0 {
            let msg = format!(
                "设置 SDR 白电平失败 (错误码 {}): {}",
                hr,
                win32_error_message(hr)
            );
            log::error!("{}", msg);
            return Err(msg);
        }
        log::info!("设置 HDR 内容亮度: {} → {:.0} nits", dn, n4);
        return Ok(());
    }
    Err(format!("未找到显示器: {}", device_name))
}

/// 查询指定显示器的 SDR 白电平（nits）
///
/// 解码：`SDRWhiteLevel` = 80 nits 的倍数 × 1000（2500 → 200 nits）
fn query_sdr_white_level(
    adapter_id: windows::Win32::Foundation::LUID,
    target_id: u32,
) -> Option<f32> {
    let mut info: DISPLAYCONFIG_SDR_WHITE_LEVEL = unsafe { std::mem::zeroed() };
    info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
    info.header.size = std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
    info.header.adapterId = adapter_id;
    info.header.id = target_id;

    let hr = unsafe { DisplayConfigGetDeviceInfo(&mut info.header as *mut _ as *mut _) };
    if hr != 0 {
        return None;
    }
    // 80 nits 倍数 × 1000 → nits
    Some(info.SDRWhiteLevel as f32 / 1000.0 * 80.0)
}

/// 查询当前所有活动的显示路径
fn query_active_paths() -> Result<Vec<DISPLAYCONFIG_PATH_INFO>, String> {
    let mut num_paths: u32 = 0;
    let mut num_modes: u32 = 0;
    let hr = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut num_paths, &mut num_modes)
    };
    if hr.0 != 0 {
        return Err(format!("GetDisplayConfigBufferSizes 失败: {}", hr.0));
    }

    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
        vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
        vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
    let hr = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut num_paths,
            paths.as_mut_ptr(),
            &mut num_modes,
            modes.as_mut_ptr(),
            None,
        )
    };
    if hr.0 != 0 {
        return Err(format!("QueryDisplayConfig 失败: {}", hr.0));
    }
    paths.truncate(num_paths as usize);
    Ok(paths)
}

/// 查询显示器高级颜色信息（HDR 支持与状态）
fn query_advanced_color(
    adapter_id: windows::Win32::Foundation::LUID,
    target_id: u32,
) -> Option<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO> {
    let mut info: DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO = unsafe { std::mem::zeroed() };
    info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
    info.header.size = std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
    info.header.adapterId = adapter_id;
    info.header.id = target_id;

    let hr = unsafe { DisplayConfigGetDeviceInfo(&mut info.header as *mut _ as *mut _) };
    if hr != 0 {
        log::warn!(
            "查询 ADVANCED_COLOR_INFO 失败: {} ({})",
            hr,
            win32_error_message(hr)
        );
        return None;
    }
    Some(info)
}

/// 查询源设备名（\\.\DISPLAY1）
fn query_source_name(
    adapter_id: windows::Win32::Foundation::LUID,
    source_id: u32,
) -> Option<String> {
    let mut info: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { std::mem::zeroed() };
    info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    info.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
    info.header.adapterId = adapter_id;
    info.header.id = source_id;

    let hr = unsafe { DisplayConfigGetDeviceInfo(&mut info.header as *mut _ as *mut _) };
    if hr != 0 {
        return None;
    }
    Some(utf16_to_string(&info.viewGdiDeviceName))
}

/// 查询显示器友好名称
fn query_target_name(
    adapter_id: windows::Win32::Foundation::LUID,
    target_id: u32,
) -> Option<String> {
    let mut info: DISPLAYCONFIG_TARGET_DEVICE_NAME = unsafe { std::mem::zeroed() };
    info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    info.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    info.header.adapterId = adapter_id;
    info.header.id = target_id;

    let hr = unsafe { DisplayConfigGetDeviceInfo(&mut info.header as *mut _ as *mut _) };
    if hr != 0 {
        return None;
    }
    Some(utf16_to_string(&info.monitorFriendlyDeviceName))
}

fn utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// 将 DisplayConfig API 返回的错误码转为可读消息
fn win32_error_message(code: i32) -> &'static str {
    match code {
        5 => "拒绝访问（可能需要管理员权限运行）",
        87 => "参数错误",
        1223 => "操作已被取消",
        _ => "未知错误",
    }
}
