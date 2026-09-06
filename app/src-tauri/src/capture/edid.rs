//! EDID 解析（面板厂商标称数据：HDR 峰值亮度 / 色域原色 / 显示器名）
//!
//! 数据源：注册表 `HKLM\SYSTEM\CurrentControlSet\Enum\DISPLAY\<instance>\Device Parameters\EDID`
//! 实例路径经 `EnumDisplayDevicesW(DeviceName)` 的 DeviceID 获取。
//!
//! 解析目标：
//! 1. **CTA-861 扩展块**（tag 0x70）内的 **HDR 静态元数据块**
//!    （Vendor Specific，OUI = IEEE 0x000040，SMPTE ST.2086）：
//!    `desired_content_max_luminance` = 厂商标称峰值（如 1000 nits）——
//!    这是其它软件（Windows 高级显示器信息等）显示的"峰值亮度"来源，
//!    通常高于 DXGI MaxLuminance（驱动按当前模式的保守报告值）。
//! 2. **基础块**（前 128 字节）：厂商 3 字母 ID、显示器名（描述符 0xFC）、
//!    色度原色 R/G/B/白点（xy 坐标，10-bit 精度）——面板标称色域。

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
use windows::Win32::UI::WindowsAndMessaging::EDD_GET_DEVICE_INTERFACE_NAME;

/// EDID 解析结果（厂商标称值）
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct EdidInfo {
    /// CTA-861 HDR 静态元数据：期望最大内容亮度（nits，厂商标称峰值）
    pub desired_max_luminance: Option<f32>,
    /// 期望最大帧平均亮度（nits）
    pub desired_max_frame_avg: Option<f32>,
    /// 期望最小亮度（nits，raw/10000）
    pub desired_min_luminance: Option<f32>,
    /// 显示器 PnP 实例名（如 CSW1663）
    pub monitor_id: String,
    /// 厂商 3 字母 ID（EDID bytes 8-9，如 "CSW"）
    pub vendor: Option<String>,
    /// 显示器名（基础块描述符 0xFC，如 "XXX Monitor"）
    pub monitor_name: Option<String>,
    /// 标称红原色 xy（基础块色度坐标）
    pub r: Option<(f32, f32)>,
    /// 标称绿原色 xy
    pub g: Option<(f32, f32)>,
    /// 标称蓝原色 xy
    pub b: Option<(f32, f32)>,
    /// 标称白点 xy
    pub white: Option<(f32, f32)>,
}

impl EdidInfo {
    /// EDID 原色是否全部有效（非零且在 xy 合理范围）
    pub fn valid_primaries(&self) -> Option<((f32, f32), (f32, f32), (f32, f32), (f32, f32))> {
        let (r, g, b, w) = (self.r?, self.g?, self.b?, self.white?);
        let ok = |p: (f32, f32)| p.0 > 0.05 && p.0 < 0.9 && p.1 > 0.0 && p.1 < 0.9;
        if ok(r) && ok(g) && ok(b) && ok(w) {
            Some((r, g, b, w))
        } else {
            None
        }
    }
}

/// 读取指定 DeviceName（\\.\DISPLAY1）的 EDID 并解析 HDR 元数据
pub fn read_edid(device_name: &str) -> Option<EdidInfo> {
    let edid = read_edid_bytes(device_name)?;
    let mut info = parse_edid(&edid);
    info.monitor_id = monitor_id_of(device_name).unwrap_or_default();
    log::info!(
        "[EDID] {} → 厂商 {:?} 名称 {:?} 标称峰值 {:?}nits 原色 R{:?} G{:?} B{:?}",
        device_name,
        info.vendor,
        info.monitor_name,
        info.desired_max_luminance,
        info.r,
        info.g,
        info.b
    );
    Some(info)
}

/// EnumDisplayDevicesW（EDD_GET_DEVICE_INTERFACE_NAME）→ 设备接口名
///
/// flags=0 时 DeviceID 是 `MONITOR\CSW1663\{类GUID}\0005`（类设备路径，
/// 注册表 Enum 树下不存在，读 EDID 必然失败）；flags=EDD_GET_DEVICE_INTERFACE_NAME
/// 时 DeviceID 是接口名 `\\?\DISPLAY#CSW1663#5&...#{GUID}`，转 # → \ 后即
/// `DISPLAY\CSW1663\5&...`——与 `HKLM\SYSTEM\CurrentControlSet\Enum` 子键一致。
fn enum_display_device(device_name: &str) -> Option<String> {
    let mut dev: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut dd = DISPLAY_DEVICEW::default();
    dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
    if !unsafe {
        EnumDisplayDevicesW(
            PCWSTR(dev.as_mut_ptr()),
            0,
            &mut dd,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    }
    .as_bool()
    {
        log::warn!("[EDID] EnumDisplayDevicesW 失败: {}", device_name);
        return None;
    }
    let len = dd
        .DeviceID
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(dd.DeviceID.len());
    Some(String::from_utf16_lossy(&dd.DeviceID[..len]))
}

/// 接口名 → 注册表 Enum 实例路径（`\\?\DISPLAY#CSW1663#5&..#{GUID}` → `DISPLAY\CSW1663\5&..`）
fn instance_path_of(interface: &str) -> Option<String> {
    let core = interface
        .strip_prefix(r"\\?\")
        .unwrap_or(interface)
        .split("#{")
        .next()?;
    if core.is_empty() {
        return None;
    }
    Some(core.replace('#', "\\"))
}

/// EnumDisplayDevicesW → DeviceID（DISPLAY\<PnP>\<inst>）→ PnP 名
fn monitor_id_of(device_name: &str) -> Option<String> {
    let path = instance_path_of(&enum_display_device(device_name)?)?;
    path.split('\\').nth(1).map(|s| s.to_string())
}

/// 注册表读原始 EDID 字节
fn read_edid_bytes(device_name: &str) -> Option<Vec<u8>> {
    let path = instance_path_of(&enum_display_device(device_name)?)?;
    // 实例路径: "DISPLAY\CSW1663\5&2ceb7072&1&UID4352" → 注册表子键路径
    let sub = format!(r"SYSTEM\CurrentControlSet\Enum\{}\Device Parameters", path);
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&sub, winreg::enums::KEY_READ)
        .ok()?;
    let val = key.get_raw_value("EDID").ok()?;
    let edid = val.bytes;
    (edid.len() >= 128).then_some(edid)
}

/// 解析 EDID：基础块（厂商/名称/原色）+ 扩展块（CTA-861 HDR 元数据）
fn parse_edid(edid: &[u8]) -> EdidInfo {
    let mut info = parse_base_block(&edid[..128.min(edid.len())]);
    let blocks = edid.len() / 128;
    for b in 1..blocks {
        let blk = &edid[b * 128..(b + 1) * 128];
        if blk[0] == 0x70 {
            parse_cta_block(blk, &mut info);
        }
    }
    info
}

/// 基础块：厂商 ID（bytes 8-9）、显示器名（描述符 0xFC）、色度原色（0x19-0x22）
fn parse_base_block(base: &[u8]) -> EdidInfo {
    let mut info = EdidInfo::default();
    if base.len() < 0x23 {
        return info;
    }

    // 厂商 3 字母 ID：两个字节编码 3 个 5-bit 字母（1-26 → 'A'-'Z'）
    let id = u16::from_be_bytes([base[8], base[9]]);
    let letter = |shift: u16| -> char { (((id >> shift) & 0x1F) as u8 + b'A' - 1) as char };
    if id != 0 {
        info.vendor = Some(format!("{}{}{}", letter(10), letter(5), letter(0)));
    }

    // 显示器名：4 个监视器描述符槽位（0x36/0x48/0x5A/0x6C），flag 0xFC
    for off in [0x36usize, 0x48, 0x5A, 0x6C] {
        if base.len() < off + 18 {
            break;
        }
        if base[off..off + 4] == [0x00, 0x00, 0x00, 0xFC] {
            let s: String = base[off + 5..off + 18]
                .iter()
                .take_while(|&&c| c != 0x0A)
                .map(|&c| c as char)
                .collect();
            let t = s.trim();
            if !t.is_empty() {
                info.monitor_name = Some(t.to_string());
            }
            break;
        }
    }

    // 色度原色（EDID 1.x §3.7，10-bit 定点 / 1024）
    let (b19, b1a) = (base[0x19] as u32, base[0x1A] as u32);
    let fx = |hi: u32, shift: u32, lo: u8| -> f32 {
        (((hi & (0x03 << shift)) << (8 - shift)) | lo as u32) as f32 / 1024.0
    };
    let r = (
        fx(b19, 6, base[0x1B]), // red-x: b19[7:6] + 0x1B
        fx(b19, 4, base[0x1C]), // red-y: b19[5:4] + 0x1C
    );
    let g = (
        fx(b19, 2, base[0x1D]), // green-x: b19[3:2] + 0x1D
        fx(b19, 0, base[0x1E]), // green-y: b19[1:0] + 0x1E
    );
    let b = (
        fx(b1a, 6, base[0x1F]), // blue-x: b1A[7:6] + 0x1F
        fx(b1a, 4, base[0x20]), // blue-y: b1A[5:4] + 0x20
    );
    let w = (
        fx(b1a, 2, base[0x21]), // white-x: b1A[3:2] + 0x21
        fx(b1a, 0, base[0x22]), // white-y: b1A[1:0] + 0x22
    );
    // 全零 = 未标称
    if r.0 > 0.0 && g.0 > 0.0 && b.0 > 0.0 {
        info.r = Some(r);
        info.g = Some(g);
        info.b = Some(b);
        info.white = Some(w);
    }

    info
}

/// CTA-861 数据块集合遍历 → 提取 HDR 静态元数据（ST.2086）
fn parse_cta_block(blk: &[u8], info: &mut EdidInfo) {
    let dtd_start = blk[2] as usize; // 数据块区结束（DTD 起点）
    let mut i = 4usize;
    while i < dtd_start && i + 1 < blk.len() {
        let hdr = blk[i];
        let tag = (hdr >> 5) & 0x07;
        let len = (hdr & 0x1F) as usize;
        if len == 0 && tag != 7 {
            break; // 零长非 vendor 块 = 集合终止
        }
        let end = (i + 1 + len).min(blk.len());
        let data = &blk[(i + 1).min(blk.len())..end];

        if tag == 7 && data.len() >= 8 {
            // HDR 静态元数据块：IEEE OUI 0x000040
            // 线上字节序两种实现都兼容（40 00 00 / 00 00 40）
            let is_hdr = (data[0] == 0x40 && data[1] == 0x00 && data[2] == 0x00)
                || (data[0] == 0x00 && data[1] == 0x00 && data[2] == 0x40);
            if is_hdr {
                // CTA-861-G §7.5.13（Static Metadata Type 1）：
                // [0-2] OUI | [3] 静态元数据类型支持位图 | [4] EOTF 支持位图
                // [5] desired_content_max_luminance（nits）
                // [6] desired_content_max_frame_avg_luminance（nits）
                // [7] desired_content_min_luminance（raw/10000 cd/m²）
                info.desired_max_luminance = Some(data[5] as f32);
                info.desired_max_frame_avg = Some(data[6] as f32);
                info.desired_min_luminance = Some(data[7] as f32 / 10000.0);
            }
        }
        i += 1 + len;
    }
}
