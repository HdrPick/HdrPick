//! 显示器亮度控制（WMI 优先 + DDC/CI 回退）
//!
//! 双路径设计：
//! 1. **WMI**（`WmiMonitorBrightnessMethods.WmiSetBrightness`）：走显示驱动，
//!    适用于笔记本内屏等驱动暴露亮度接口的显示器。仅 DDC/CI 的实现在这类
//!    显示器上会被静默忽略（SetVCPFeature 返回成功但亮度不变）。
//! 2. **DDC/CI**（VCP code 0x10）：走 I2C 物理总线，适用于 HDMI/DP/USB-C
//!    外接显示器。
//!
//! 显示器与 WMI 实例的关联：WMI `InstanceName`（`DISPLAY\CSW1663\...`）与
//! GDI `EnumDisplayDevicesW` 的 `DeviceID`（`MONITOR\CSW1663\...`）通过
//! 显示器硬件 ID（如 CSW1663）关联。
//!
//! WMI 是 COM 调用，在常驻 STA 工作线程上执行（CoInitializeEx +
//! CoInitializeSecurity 只做一次，线程永不退出——若线程退出且 COM 卸载，
//! 进程安全设置会失效，后续连接将报 0x80041003）。

use std::sync::Mutex;

use windows::core::{BSTR, HSTRING, PCWSTR, VARIANT};
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitor, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
    PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, DISPLAY_DEVICEW, HDC, HMONITOR,
    MONITORINFOEXW,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoInitializeSecurity, CoSetProxyBlanket,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator, WBEM_FLAG_FORWARD_ONLY,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

/// VCP 0x10 = 亮度
const VCP_BRIGHTNESS: u8 = 0x10;

/// 单个显示器的亮度状态
#[derive(Debug, Clone, serde::Serialize)]
pub struct MonitorBrightness {
    /// GDI 设备名（如 \\.\DISPLAY1）
    pub device_name: String,
    /// 是否主显示器
    pub is_primary: bool,
    /// 当前亮度（0-100）
    pub current: u32,
    /// 亮度上限（通常 100）
    pub max: u32,
    /// 亮度控制是否支持（WMI 或 DDC/CI 任一可用）
    pub supported: bool,
    /// 使用的控制路径："wmi" | "ddc" | ""
    pub via: String,
}

/// 显示器枚举中间结构
struct MonitorEntry {
    device_name: String,
    is_primary: bool,
    hmonitor: isize,
    /// 设备实例路径（如 DISPLAY\CSW1663\5&2ceb7072&1&UID4352），用于 WMI 匹配
    device_id: String,
}

/// WMI 亮度实例
#[derive(Clone)]
struct WmiBrightnessEntry {
    instance_name: String,
    current: u32,
}

// ==========================================================================
// 公开接口
// ==========================================================================

/// 预热 WMI：启动常驻线程并**等待进程级 COM 安全注册完成**。
///
/// 必须在 WebView2 初始化之前调用（main 线程入口处）：
/// WebView2 会抢先调用 CoInitializeSecurity（低模拟级别 + 受限认证
/// 服务列表），此后任何 CoSetProxyBlanket 都无法补救（WMI 报
/// 0x80041003）。我们先注册 IMPERSONATE，WebView2 的调用会得到
/// RPC_E_TOO_LATE——它对此有容忍处理（.NET CLR 宿主即此模式）。
pub fn warmup() {
    match wmi_sender() {
        Ok(_) => log::info!("WMI 预热完成（进程 COM 安全已注册为 IMPERSONATE）"),
        Err(e) => log::warn!("WMI 预热失败: {}", e),
    }
}

/// 查询所有显示器的亮度
pub fn get_brightness_states() -> Result<Vec<MonitorBrightness>, String> {
    let monitors = enum_monitors()?;
    let wmi_entries = wmi_read_all();

    let mut result = Vec::new();
    for m in &monitors {
        // 优先 WMI：实例名与显示器硬件 ID 匹配
        let wmi_hit = wmi_entries
            .iter()
            .find(|w| wmi_instance_matches(&w.instance_name, &m.device_id));

        if let Some(w) = wmi_hit {
            result.push(MonitorBrightness {
                device_name: m.device_name.clone(),
                is_primary: m.is_primary,
                current: w.current,
                max: 100,
                supported: true,
                via: "wmi".into(),
            });
            continue;
        }

        // 回退 DDC/CI
        let hmon = HMONITOR(m.hmonitor as *mut _);
        let (supported, current, max) = read_brightness_ddc(hmon);
        result.push(MonitorBrightness {
            device_name: m.device_name.clone(),
            is_primary: m.is_primary,
            current,
            max,
            supported,
            via: if supported {
                "ddc".into()
            } else {
                String::new()
            },
        });
    }
    Ok(result)
}

/// 设置指定显示器亮度（level: 0-100）
pub fn set_brightness(device_name: &str, level: u32) -> Result<(), String> {
    let monitors = enum_monitors()?;
    let m = monitors
        .iter()
        .find(|m| m.device_name == device_name)
        .ok_or_else(|| format!("未找到显示器: {}", device_name))?;

    let level = level.clamp(0, 100);

    // 优先 WMI
    let wmi_hit = wmi_find_instance(&m.device_id).unwrap_or_else(|e| {
        log::warn!("WMI 查找亮度实例失败: {}", e);
        None
    });
    if let Some(instance_name) = wmi_hit {
        log::info!("亮度设置(WMI): {} → {}", instance_name, level);
        return wmi_set_brightness(&instance_name, level);
    }

    // 回退 DDC/CI
    log::info!("亮度设置(DDC/CI): {} → {}", m.device_name, level);
    let physical = get_physical_monitor(HMONITOR(m.hmonitor as *mut _))?;
    let hr = unsafe { SetVCPFeature(physical.hPhysicalMonitor, VCP_BRIGHTNESS, level) };
    unsafe { DestroyPhysicalMonitor(physical.hPhysicalMonitor).ok() };
    if hr != 0 {
        return Err(format!(
            "SetVCPFeature 失败 (错误码 {}): {}",
            hr,
            if hr == 5 {
                "拒绝访问"
            } else {
                "显示器不支持 DDC/CI 或通信失败"
            }
        ));
    }
    Ok(())
}

// ==========================================================================
// WMI 实现（常驻 STA 工作线程）
// ==========================================================================

/// WMI 任务：在常驻线程上自行建立连接并执行，返回类型擦除的结果
/// （连接在闭包内建立，连接失败时返回 Err<T, String>，保证类型一致）
type WmiJob = Box<dyn FnOnce() -> Box<dyn std::any::Any + Send> + Send>;

/// 带回复通道的任务（结果发回调用方）
struct WmiRequest {
    job: WmiJob,
    reply: std::sync::mpsc::Sender<Box<dyn std::any::Any + Send>>,
}

/// 常驻 WMI 线程的任务通道（惰性启动；线程进程内常驻）
static WMI_TX: Mutex<Option<std::sync::mpsc::Sender<WmiRequest>>> = Mutex::new(None);

/// 进程级 COM 安全初始化（每进程一次，在常驻线程上调用）
///
/// 微软官方 WMI 客户端示例的标准步骤：进程默认模拟级别必须是
/// Impersonate，否则 WMI 调用报 0x80041003 (WBEM_E_ACCESS_DENIED)。
/// 必须在 CoInitializeEx **之后**、任何 COM 网络调用之前调用。
/// 常驻线程不退出、不调用 CoUninitialize，COM 库与安全设置全程有效
/// （CoUninitialize 若使 COM 卸载，安全设置会被清除——这是单次
/// spawn 线程方案失败的原因）。
fn ensure_process_com_security() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        if let Err(e) = CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_DEFAULT,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
            None,
        ) {
            // RPC_E_TOO_LATE（进程已初始化安全）是预期情况，其余记录日志
            log::info!("CoInitializeSecurity 失败: {}（进程已初始化则正常）", e);
        }
    });
}

/// 在常驻 WMI 线程上执行任务
///
/// 线程惰性启动一次，之后所有亮度读写复用同一线程（COM/安全设置常驻）。
/// 每次任务建立全新 IWbemServices 连接（连接轻量、状态干净）。
fn with_wmi<T: Send + 'static>(
    f: impl FnOnce(&IWbemServices) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    // 1. 获取常驻线程的任务通道（无则启动线程）
    let sender = wmi_sender()?;

    // 2. 包装任务 + 响应通道（连接在 WMI 线程上建立，
    //    失败时返回 Err<T, String>——与成功路径类型一致，downcast 不丢错误）
    let (rtx, rrx) = std::sync::mpsc::channel::<Box<dyn std::any::Any + Send>>();
    let job: WmiJob = Box::new(move || match connect_wmi() {
        Ok(services) => Box::new(f(&services)) as _,
        Err(e) => Box::new(Err::<T, String>(e)) as _,
    });
    sender
        .send(WmiRequest { job, reply: rtx })
        .map_err(|_| "WMI 工作线程已退出".to_string())?;

    // 3. 等待结果并还原类型
    let boxed = rrx.recv().map_err(|_| "WMI 工作线程无响应".to_string())?;
    match boxed.downcast::<Result<T, String>>() {
        Ok(r) => *r,
        Err(_) => Err("WMI 结果类型不匹配".to_string()),
    }
}

/// 获取（或启动）常驻 WMI 线程的发送端
fn wmi_sender() -> Result<std::sync::mpsc::Sender<WmiRequest>, String> {
    let mut guard = WMI_TX.lock().map_err(|_| "WMI 通道锁中毒".to_string())?;
    if let Some(tx) = guard.as_ref() {
        return Ok(tx.clone());
    }
    let tx = spawn_wmi_thread()?;
    *guard = Some(tx.clone());
    Ok(tx)
}

/// 启动常驻 WMI 工作线程
///
/// 返回前等待 COM 安全初始化完成信号（warmup 需要确保
/// CoInitializeSecurity 已注册后才让 main 继续——否则无法
/// 保证抢在 WebView2 之前）
fn spawn_wmi_thread() -> Result<std::sync::mpsc::Sender<WmiRequest>, String> {
    let (tx, rx) = std::sync::mpsc::channel::<WmiRequest>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<bool>();
    std::thread::Builder::new()
        .name("jietu-wmi".into())
        .spawn(move || {
            // COM 初始化 + 进程安全（顺序：先 CoInitializeEx 再 CoInitializeSecurity）
            unsafe {
                let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                if hr.is_err() {
                    log::error!("WMI 线程 CoInitializeEx 失败: {:#010x}", hr.0);
                }
            }
            ensure_process_com_security();
            // 通知等待方：进程 COM 安全已注册
            let _ = init_tx.send(true);

            // 请求循环：所有 sender drop（进程退出）时结束
            while let Ok(req) = rx.recv() {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (req.job)()));
                let payload = match result {
                    Ok(b) => b,
                    Err(_) => Box::new(Err::<String, String>("WMI 任务 panic".into())),
                };
                let _ = req.reply.send(payload);
            }
        })
        .map_err(|e| format!("启动 WMI 线程失败: {}", e))?;
    // 等待 COM 安全注册完成（上限 2 秒，防止卡死启动）
    let _ = init_rx.recv_timeout(std::time::Duration::from_secs(2));
    Ok(tx)
}

/// 建立全新的 WMI 连接（ROOT\WMI + 安全毯）
fn connect_wmi() -> Result<IWbemServices, String> {
    unsafe {
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("创建 WbemLocator 失败: {}", e))?;
        let empty = BSTR::new();
        let services = locator
            .ConnectServer(
                &BSTR::from("ROOT\\WMI"),
                &empty,
                &empty,
                &empty,
                0,
                &empty,
                None,
            )
            .map_err(|e| format!("连接 WMI 失败: {}", e))?;
        // 代理安全毯：Impersonate 模拟级别（WMI 必需）
        set_wmi_proxy_security(&services)?;
        Ok(services)
    }
}

/// 对 WMI 代理设置安全毯（认证 NTLM + Impersonate 模拟级别）
///
/// 微软官方 WMI 客户端示例的标准步骤；PowerShell/.NET 内部自动完成。
/// 对 services 与枚举器代理均需设置（各自独立的代理对象）。
///
/// 教训（2026-08-23 排查记录）：
/// - pAuthInfo 必须传 None。显式填 SEC_WINNT_AUTH_IDENTITY（含空密码）
///   会被当作"字面空密码凭据"认证，CoSetProxyBlanket 直接 0x80070005。
/// - 认证级别 CALL 即可（与 PowerShell/.NET 一致）。
fn set_wmi_proxy_security<T: windows::core::Interface>(proxy: &T) -> Result<(), String> {
    // CoSetProxyBlanket 参数要求 IUnknown；对 IUnknown 设置可同时
    // 覆盖该代理管理器下所有接口的安全毯
    let unknown: windows::core::IUnknown = proxy
        .cast()
        .map_err(|e| format!("cast IUnknown 失败: {}", e))?;
    unsafe {
        CoSetProxyBlanket(
            &unknown,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
        .map_err(|e| format!("CoSetProxyBlanket 失败: {}", e))
    }
}

/// 从 EnumDisplayDevicesW 的 DeviceID 提取显示器硬件 ID
///
/// DeviceID 格式为 `MONITOR\CSW1663\{4d36e96e-...}\0005`，第二段（如 CSW1663）
/// 是显示器型号硬件 ID；WMI InstanceName 格式为 `DISPLAY\CSW1663\5&...&UID4352_0`，
/// 两者通过该硬件 ID 关联（前缀 MONITOR\ vs DISPLAY\ 不同，不能直接前缀匹配）。
fn monitor_hardware_id(device_id: &str) -> &str {
    device_id.split('\\').nth(1).unwrap_or("")
}

/// WMI 实例名是否属于该显示器（硬件 ID 子串匹配，忽略大小写）
fn wmi_instance_matches(instance_name: &str, device_id: &str) -> bool {
    let hw = monitor_hardware_id(device_id);
    if hw.is_empty() {
        return false;
    }
    instance_name.to_uppercase().contains(&hw.to_uppercase())
}

/// 查询所有 WmiMonitorBrightness 实例（InstanceName + 当前亮度）
fn wmi_read_all() -> Vec<WmiBrightnessEntry> {
    let result = with_wmi(|services| {
        let enumerator = unsafe {
            services.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT * FROM WmiMonitorBrightness"),
                WBEM_FLAG_FORWARD_ONLY,
                None,
            )
        }
        .map_err(|e| format!("查询 WmiMonitorBrightness 失败: {}", e))?;
        // 枚举器是独立代理，同样需要安全毯
        set_wmi_proxy_security(&enumerator)?;

        let mut entries = Vec::new();
        loop {
            let mut objs: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;
            let hr = unsafe { enumerator.Next(-1, &mut objs, &mut returned) };
            if hr.is_err() || returned == 0 {
                break;
            }
            if let Some(obj) = objs[0].take() {
                let name = wmi_get_string(&obj, "InstanceName").unwrap_or_default();
                let current = wmi_get_u32(&obj, "CurrentBrightness").unwrap_or(0);
                if !name.is_empty() {
                    entries.push(WmiBrightnessEntry {
                        instance_name: name,
                        current,
                    });
                }
            }
        }
        Ok(entries)
    });
    match result {
        Ok(v) => v,
        Err(e) => {
            log::warn!("WMI 读取亮度失败: {}", e);
            Vec::new()
        }
    }
}

/// 查找 DeviceID 对应的 WmiMonitorBrightnessMethods 实例名
/// （错误返回给调用方记录；None = WMI 可用但无匹配实例）
fn wmi_find_instance(device_id: &str) -> Result<Option<String>, String> {
    let device_id = device_id.to_string();
    with_wmi(move |services| {
        let enumerator = unsafe {
            services.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT * FROM WmiMonitorBrightnessMethods"),
                WBEM_FLAG_FORWARD_ONLY,
                None,
            )
        }
        .map_err(|e| format!("查询 WmiMonitorBrightnessMethods 失败: {}", e))?;
        // 枚举器是独立代理，同样需要安全毯
        set_wmi_proxy_security(&enumerator)?;

        let mut found: Option<String> = None;
        loop {
            let mut objs: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;
            let hr = unsafe { enumerator.Next(-1, &mut objs, &mut returned) };
            if hr.is_err() || returned == 0 {
                break;
            }
            if let Some(obj) = objs[0].take() {
                let name = wmi_get_string(&obj, "InstanceName").unwrap_or_default();
                if wmi_instance_matches(&name, &device_id) {
                    found = Some(name);
                    break;
                }
            }
        }
        Ok(found)
    })
}

/// 通过 WmiSetBrightness 设置亮度
fn wmi_set_brightness(instance_name: &str, level: u32) -> Result<(), String> {
    let instance_name = instance_name.to_string();
    with_wmi(move |services| {
        // 对象路径：WmiMonitorBrightnessMethods.InstanceName="..."
        // 注意：值中的反斜杠必须转义为 \\（WMI 对象路径语法）
        let escaped = instance_name.replace('\\', "\\\\");
        let path = BSTR::from(format!(
            "WmiMonitorBrightnessMethods.InstanceName=\"{}\"",
            escaped
        ));
        let method = BSTR::from("WmiSetBrightness");

        // 取类定义（方法签名在类上，不在实例上）→ SpawnInstance 构造输入参数
        let class_path = BSTR::from("WmiMonitorBrightnessMethods");
        let mut class_obj: Option<IWbemClassObject> = None;
        unsafe {
            services
                .GetObject(
                    &class_path,
                    Default::default(),
                    None,
                    Some(&mut class_obj),
                    None,
                )
                .map_err(|e| format!("获取类定义失败: {}", e))?;
        }
        let class_obj = class_obj.ok_or_else(|| "类定义对象为空".to_string())?;

        let mut in_sig: Option<IWbemClassObject> = None;
        let mut out_sig: Option<IWbemClassObject> = None;
        unsafe {
            class_obj
                .GetMethod(&method, 0, &mut in_sig, &mut out_sig)
                .map_err(|e| format!("获取方法签名失败: {}", e))?;
        }
        let in_sig = in_sig.ok_or_else(|| "无输入参数签名".to_string())?;
        let params = unsafe {
            in_sig
                .SpawnInstance(0)
                .map_err(|e| format!("创建参数实例失败: {}", e))?
        };
        // Brightness 参数在 WMI 类中定义为 uint8（VT_UI1），
        // 传 u32（VT_UI4）会报 0x80041005 类型不匹配
        let brightness = VARIANT::from(level as u8);
        unsafe {
            params
                .Put(&HSTRING::from("Brightness"), 0, &brightness, 0)
                .map_err(|e| format!("设置参数失败: {}", e))?;
        }
        // Timeout 参数（uint32，已弃用）虽文档标记废弃，但 WDM 提供程序
        // 要求必须提供——缺失时报 0x80041008/0x8004102f 无效参数。
        // 注意：CIM uint32 在 VARIANT 层映射为 VT_I4（有符号），传 VT_UI4 报类型不匹配
        let timeout = VARIANT::from(0i32);
        unsafe {
            params
                .Put(&HSTRING::from("Timeout"), 0, &timeout, 0)
                .map_err(|e| format!("设置 Timeout 参数失败: {}", e))?;
        }

        // 执行方法（检查输出参数 ReturnValue）
        let mut out_params: Option<IWbemClassObject> = None;
        unsafe {
            services
                .ExecMethod(
                    &path,
                    &method,
                    Default::default(),
                    None,
                    &params,
                    Some(&mut out_params),
                    None,
                )
                .map_err(|e| format!("WmiSetBrightness 执行失败: {}", e))?;
        }
        if let Some(out) = &out_params {
            if let Ok(rv) = wmi_get_u32(out, "ReturnValue") {
                if rv != 0 {
                    return Err(format!("WmiSetBrightness 返回错误码: {}", rv));
                }
            }
        }
        Ok(())
    })
}

/// 从 IWbemClassObject 读字符串属性
fn wmi_get_string(obj: &IWbemClassObject, prop: &str) -> Result<String, String> {
    let mut val = VARIANT::new();
    unsafe {
        obj.Get(&HSTRING::from(prop), 0, &mut val, None, None)
            .map_err(|e| format!("读取属性 {} 失败: {}", prop, e))?;
    }
    let s: BSTR = (&val)
        .try_into()
        .map_err(|e| format!("属性 {} 不是字符串: {}", prop, e))?;
    Ok(s.to_string())
}

/// 从 IWbemClassObject 读 u32 属性
fn wmi_get_u32(obj: &IWbemClassObject, prop: &str) -> Result<u32, String> {
    let mut val = VARIANT::new();
    unsafe {
        obj.Get(&HSTRING::from(prop), 0, &mut val, None, None)
            .map_err(|e| format!("读取属性 {} 失败: {}", prop, e))?;
    }
    (&val)
        .try_into()
        .map_err(|e| format!("属性 {} 不是整数: {}", prop, e))
}

// ==========================================================================
// 显示器枚举 + DDC/CI
// ==========================================================================

/// 枚举所有桌面上显示器（设备名 + HMONITOR + DeviceID）
fn enum_monitors() -> Result<Vec<MonitorEntry>, String> {
    // 回调在同一线程同步执行，用局部 Vec 通过 LPARAM 传指针
    let mut collected: Vec<MonitorEntry> = Vec::new();
    let ptr = &mut collected as *mut Vec<MonitorEntry>;

    let ok = unsafe { EnumDisplayMonitors(None, None, Some(enum_proc), LPARAM(ptr as isize)) };
    if !ok.as_bool() {
        return Err("EnumDisplayMonitors 失败".into());
    }

    // 为每个显示器补充 DeviceID（EnumDisplayDevicesW：适配器名 → 显示器设备）
    for m in collected.iter_mut() {
        let adapter = to_wide(&m.device_name);
        let adapter_ptr = PCWSTR(adapter.as_ptr());
        // 每个适配器下枚举显示器设备，取第一个 ACTIVE 的
        let mut i: u32 = 0;
        loop {
            let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if unsafe { EnumDisplayDevicesW(adapter_ptr, i, &mut dd, 0) }.as_bool() {
                const DISPLAY_DEVICE_ACTIVE: u32 = 0x1;
                if dd.StateFlags & DISPLAY_DEVICE_ACTIVE != 0 {
                    m.device_id = wide_to_string(&dd.DeviceID);
                    break;
                }
                i += 1;
            } else {
                break;
            }
        }
    }

    Ok(collected)
}

extern "system" fn enum_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let collected = unsafe { &mut *(lparam.0 as *mut Vec<MonitorEntry>) };
    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut _) }.as_bool() {
        let name = wide_to_string(&info.szDevice);
        let is_primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;
        collected.push(MonitorEntry {
            device_name: name,
            is_primary,
            hmonitor: hmonitor.0 as isize,
            device_id: String::new(),
        });
    }
    BOOL(1)
}

/// 读取亮度（DDC/CI）→ (supported, current, max)
fn read_brightness_ddc(hmonitor: HMONITOR) -> (bool, u32, u32) {
    let physical = match get_physical_monitor(hmonitor) {
        Ok(p) => p,
        Err(_) => return (false, 0, 100),
    };

    let mut current: u32 = 0;
    let mut max: u32 = 100;
    let hr = unsafe {
        GetVCPFeatureAndVCPFeatureReply(
            physical.hPhysicalMonitor,
            VCP_BRIGHTNESS,
            None,
            &mut current,
            Some(&mut max),
        )
    };
    unsafe { DestroyPhysicalMonitor(physical.hPhysicalMonitor).ok() };

    if hr != 0 || max == 0 {
        return (false, 0, 100);
    }
    (true, current, max)
}

/// 获取物理显示器句柄（调用方负责 DestroyPhysicalMonitor）
fn get_physical_monitor(hmonitor: HMONITOR) -> Result<PHYSICAL_MONITOR, String> {
    let mut count: u32 = 0;
    unsafe {
        GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count)
            .map_err(|e| format!("GetNumberOfPhysicalMonitors 失败: {}", e))?;
    }
    if count == 0 {
        return Err("无物理显示器句柄".into());
    }

    let mut monitors: Vec<PHYSICAL_MONITOR> = vec![unsafe { std::mem::zeroed() }; count as usize];
    unsafe {
        GetPhysicalMonitorsFromHMONITOR(hmonitor, &mut monitors)
            .map_err(|e| format!("GetPhysicalMonitors 失败: {}", e))?;
    }
    Ok(monitors[0])
}

// ==========================================================================
// 工具
// ==========================================================================

fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn to_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::Interface;
    use windows::Win32::System::Com::{
        RPC_C_AUTHN_LEVEL, RPC_C_AUTHN_LEVEL_CALL, RPC_C_AUTHN_LEVEL_PKT,
        RPC_C_AUTHN_LEVEL_PKT_PRIVACY, RPC_C_IMP_LEVEL_IDENTIFY,
    };
    use windows::Win32::System::Rpc::{RPC_C_AUTHN_DEFAULT, RPC_C_AUTHN_GSS_NEGOTIATE};

    /// 方案 B 验证：我们先注册 IMPERSONATE（warmup 抢先），
    /// "WebView2"后到的 IDENTIFY 注册失败（RPC_E_TOO_LATE），
    /// 之后 WMI 查询应成功。
    /// 单独运行：cargo test --lib our_security_first -- --nocapture
    #[test]
    fn our_security_first() {
        // 1. 模拟 warmup：我们的线程先注册 IMPERSONATE
        std::thread::spawn(|| {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let r = CoInitializeSecurity(
                    None,
                    -1,
                    None,
                    None,
                    RPC_C_AUTHN_LEVEL_DEFAULT,
                    RPC_C_IMP_LEVEL_IMPERSONATE,
                    None,
                    EOAC_NONE,
                    None,
                );
                eprintln!("[first] 我方 CoInitializeSecurity: {:?}", r.map(|_| "成功"));
            }
            std::thread::sleep(std::time::Duration::from_secs(600));
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        // 2. 模拟 WebView2：后到的 IDENTIFY 注册（预期 RPC_E_TOO_LATE）
        let webview = std::thread::spawn(|| unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let r = CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_DEFAULT,
                RPC_C_IMP_LEVEL_IDENTIFY,
                None,
                EOAC_NONE,
                None,
            );
            match &r {
                Ok(()) => eprintln!("[first] WebView2 模拟注册竟然成功（异常）"),
                Err(e) => eprintln!("[first] WebView2 模拟注册失败（预期）: {}", e),
            }
        });
        webview.join().unwrap();

        // 3. 验证 WMI 查询成功
        let entries = wmi_read_all();
        eprintln!("[first] wmi_read_all = {} 条", entries.len());
        for e in &entries {
            eprintln!("[first]   实例: {} current={}", e.instance_name, e.current);
        }
        assert!(!entries.is_empty(), "WMI 查询应成功（我方安全注册生效）");
    }

    /// 复现环境下的安全毯矩阵诊断：尝试 认证服务 × 认证级别 × 凭据
    /// 的组合，找出在"进程 COM 安全被抢先（IDENTIFY）"环境下能通过
    /// WMI 认证的有效组合。
    /// 单独运行：cargo test --lib foreign_matrix -- --nocapture
    #[test]
    fn foreign_matrix() {
        // 模拟 WebView2：抢先以 IDENTIFY（低级别）初始化进程 COM 安全
        std::thread::spawn(|| {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let _ = CoInitializeSecurity(
                    None,
                    -1,
                    None,
                    None,
                    RPC_C_AUTHN_LEVEL_DEFAULT,
                    RPC_C_IMP_LEVEL_IDENTIFY,
                    None,
                    EOAC_NONE,
                    None,
                );
            }
            std::thread::sleep(std::time::Duration::from_secs(600));
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        let handle = std::thread::spawn(|| {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            // 组合矩阵：认证服务 × 认证级别
            let combos: Vec<(u32, RPC_C_AUTHN_LEVEL)> = vec![
                (RPC_C_AUTHN_WINNT, RPC_C_AUTHN_LEVEL_CALL),
                (RPC_C_AUTHN_WINNT, RPC_C_AUTHN_LEVEL_PKT),
                (RPC_C_AUTHN_WINNT, RPC_C_AUTHN_LEVEL_PKT_PRIVACY),
                (RPC_C_AUTHN_DEFAULT as u32, RPC_C_AUTHN_LEVEL_CALL),
                (RPC_C_AUTHN_DEFAULT as u32, RPC_C_AUTHN_LEVEL_PKT_PRIVACY),
                (RPC_C_AUTHN_GSS_NEGOTIATE, RPC_C_AUTHN_LEVEL_PKT_PRIVACY),
            ];
            for (authn, level) in combos {
                let outcome = unsafe { try_wmi_query(authn, level) };
                eprintln!("[matrix] authn={} level={} → {}", authn, level.0, outcome);
            }
        });
        handle.join().unwrap();
    }

    /// 单次 WMI 查询尝试（诊断用）：建连接 → 设毯 → ExecQuery
    unsafe fn try_wmi_query(authn: u32, level: RPC_C_AUTHN_LEVEL) -> String {
        let locator: IWbemLocator = match CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
        {
            Ok(l) => l,
            Err(e) => return format!("CoCreateInstance 失败: {}", e),
        };
        let empty = BSTR::new();
        let services = match locator.ConnectServer(
            &BSTR::from("ROOT\\WMI"),
            &empty,
            &empty,
            &empty,
            0,
            &empty,
            None,
        ) {
            Ok(s) => s,
            Err(e) => return format!("ConnectServer 失败: {}", e),
        };
        let unknown: windows::core::IUnknown = services.cast().expect("cast IUnknown");
        if let Err(e) = CoSetProxyBlanket(
            &unknown,
            authn,
            RPC_C_AUTHZ_NONE,
            PCWSTR::null(),
            level,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        ) {
            return format!("CoSetProxyBlanket 失败: {}", e);
        }
        match services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from("SELECT * FROM WmiMonitorBrightness"),
            WBEM_FLAG_FORWARD_ONLY,
            None,
        ) {
            Ok(_) => "ExecQuery 成功".to_string(),
            Err(e) => format!("ExecQuery 失败: {}", e),
        }
    }

    /// 复现 Tauri 环境：另一组件（WebView2）抢先 CoInitializeSecurity
    /// （低模拟级别），之后 WMI 线程的 CoInitializeSecurity 报 RPC_E_TOO_LATE，
    /// WMI 查询报 0x80041003。
    /// 单独运行：cargo test --lib foreign_security -- --nocapture
    #[test]
    fn foreign_security_repro() {
        // 模拟 WebView2：抢先以 IDENTIFY（低级别）初始化进程 COM 安全
        std::thread::spawn(|| {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let hr = CoInitializeSecurity(
                    None,
                    -1,
                    None,
                    None,
                    RPC_C_AUTHN_LEVEL_DEFAULT,
                    RPC_C_IMP_LEVEL_IDENTIFY,
                    None,
                    EOAC_NONE,
                    None,
                );
                match &hr {
                    Ok(()) => eprintln!("[repro] 抢先 CoInitializeSecurity 成功"),
                    Err(e) => eprintln!("[repro] 抢先 CoInitializeSecurity 失败: {}", e),
                }
            }
            // 阻塞线程，保持 COM 状态存活（模拟 WebView2 常驻）
            std::thread::sleep(std::time::Duration::from_secs(600));
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        // 此时 WMI 线程启动：CoInitializeSecurity 必然 RPC_E_TOO_LATE（0x80010119）
        let entries = wmi_read_all();
        eprintln!("[repro] wmi_read_all = {} 条", entries.len());
        for e in &entries {
            eprintln!("[repro]   实例: {} current={}", e.instance_name, e.current);
        }
        // 预期复现：Tauri 环境下此处为 0 条（0x80041003）
        assert!(
            !entries.is_empty(),
            "复现失败：WMI 查询竟然成功（未能模拟 Tauri 环境）"
        );
    }

    /// WMI 链路回归测试（cargo test --lib wmi_diag -- --nocapture）
    /// 验证：COM 安全顺序 → WMI 查询 → 硬件 ID 匹配 → 状态聚合
    #[test]
    fn wmi_diag() {
        let monitors = enum_monitors().expect("enum_monitors 失败");
        assert!(!monitors.is_empty(), "应至少枚举到一个显示器");
        for m in &monitors {
            eprintln!(
                "[diag] 显示器: name={} primary={} device_id='{}' 硬件ID='{}'",
                m.device_name,
                m.is_primary,
                m.device_id,
                monitor_hardware_id(&m.device_id)
            );
            assert!(
                !monitor_hardware_id(&m.device_id).is_empty(),
                "DeviceID 应包含硬件 ID"
            );
        }

        let entries = wmi_read_all();
        eprintln!("[diag] wmi_read_all = {} 条", entries.len());
        for e in &entries {
            eprintln!("[diag]   实例: {} current={}", e.instance_name, e.current);
        }

        for m in &monitors {
            match wmi_find_instance(&m.device_id) {
                Ok(inst) => eprintln!(
                    "[diag] find_instance('{}') = {:?}",
                    monitor_hardware_id(&m.device_id),
                    inst
                ),
                Err(e) => eprintln!("[diag] find_instance 错误: {}", e),
            }
        }

        let states = get_brightness_states().expect("get_brightness_states 失败");
        for s in &states {
            eprintln!(
                "[diag] 状态: {} supported={} via={} current={}/{}",
                s.device_name, s.supported, s.via, s.current, s.max
            );
        }

        // 验证 WMI 写路径（读当前值 → 设置新值 → 读回 → 恢复）
        let (m, before) = match (monitors.first(), states.first()) {
            (Some(m), Some(s)) if s.via == "wmi" => (m, s.current),
            _ => {
                eprintln!("[diag] 无 WMI 显示器，跳过写路径验证");
                return;
            }
        };
        let target = if before >= 50 {
            before - 20
        } else {
            before + 20
        };
        set_brightness(&m.device_name, target).expect("set_brightness(WMI) 失败");
        let after = wmi_read_all()
            .first()
            .map(|e| e.current)
            .unwrap_or(u32::MAX);
        eprintln!(
            "[diag] 亮度写路径: {} → {} (读回 {})",
            before, target, after
        );
        // 恢复原值
        let _ = set_brightness(&m.device_name, before);
        assert_eq!(after, target, "WMI 设置亮度后读回应等于目标值");
    }
}
