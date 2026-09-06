//! 文件关联 + 资源管理器右键菜单（设计稿 3.3 / 七·资源管理器集成）
//!
//! 全部写 HKCU（当前用户），无需管理员权限。
//!
//! 注册内容：
//! 1. ProgID `jietu.hdr.Image`（图片）/ `jietu.hdr.Video`（视频）：
//!    类型描述 / 图标 / open 命令
//! 2. 每个支持扩展名的 `OpenWithProgids` → 出现在「打开方式」候选
//! 3. RegisteredApplications + Capabilities → 出现在 Win11「设置·默认应用」
//! 4. 右键 verb（经典注册表命令，Win11 显示在「显示更多选项」二级菜单，
//!    Win10 直接显示；顶层新菜单需 MSIX sparse package，见文档 9.3）：
//!    - 图片：用 jietu-hdr 打开 / 添加到 jietu 隐私相册
//!    - 视频：用 jietu-hdr 播放
//!    - 目录与目录背景：用 jietu-hdr 浏览此文件夹
//!
//! exe 命令行约定（lib.rs handle_launch_args 路由）：
//!   `"jietu-hdr.exe" "<图片路径>"`          → 看图窗口打开
//!   `"jietu-hdr.exe" "<视频路径>"`          → 独立播放器窗口
//!   `"jietu-hdr.exe" --library "<目录>"`    → 相册页挂载并浏览
//!   `"jietu-hdr.exe" --vault-add <路径...>` → 加入隐私相册

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

/// ProgID（应用注册的类型标识）
const PROG_ID: &str = "jietu.hdr.Image";
/// 视频 ProgID（独立播放器；与图片分离——默认应用页可分别设置）
const VIDEO_PROG_ID: &str = "jietu.hdr.Video";
/// RegisteredApplications 登记名
const APP_NAME: &str = "jietu-hdr";
/// Capabilities 注册表路径（HKCU 相对）
const CAPABILITIES_KEY: &str = r"Software\jietu-hdr\Capabilities";

/// 右键 verb 的注册表路径（shell 子键相对 Software\Classes）
const VERB_IMAGE_OPEN: &str = r"SystemFileAssociations\image\shell\jietu.open";
const VERB_IMAGE_VAULT: &str = r"SystemFileAssociations\image\shell\jietu.vault";
/// PDF 的 PerceivedType 不是 image，挂在 .pdf 扩展名键下才能出右键菜单
const VERB_PDF_OPEN: &str = r"SystemFileAssociations\.pdf\shell\jietu.open";
/// 视频 PerceivedType 组右键（mp4/mkv/avi/mov/webm 等系统默认 PerceivedType=video）
const VERB_VIDEO_PLAY: &str = r"SystemFileAssociations\video\shell\jietu.play";
const VERB_DIR_BROWSE: &str = r"Directory\shell\jietu.browse";
const VERB_DIR_BG_BROWSE: &str = r"Directory\Background\shell\jietu.browse";

/// 支持关联的图片扩展名（与 viewer 格式清单一致）
pub const ASSOC_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "avif", "tif", "tiff", "ico", "psd", "jxl", "jxr",
    "wdp", "exr", "pdf",
];
/// 支持关联的视频扩展名（与 lib.rs VIDEO_LAUNCH_EXTS 一致：双击/「打开方式」→ 独立播放器）
pub const VIDEO_ASSOC_EXTS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "flv", "ts", "m4v"];

/// 当前 exe 完整路径（加引号形式用于注册表命令）
fn exe_quoted() -> String {
    format!(
        "\"{}\"",
        std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    )
}

// ==================== 写入辅助 ====================

/// 打开/创建 HKCU 子键
///
/// 注意：已存在的键也必须以读写权限打开——注册流程是幂等覆盖写，
/// 只读句柄上 set_value 会报 os error 5（拒绝访问）。
fn open_or_create(sub: &str) -> Result<RegKey, String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(sub, KEY_READ | KEY_WRITE)
        .or_else(|_| hkcu.create_subkey(sub).map(|(k, _)| k))
        .map_err(|e| format!("打开注册表键 HKCU\\{} 失败: {}", sub, e))
}

/// 删除 HKCU 子键（不存在视为成功）
fn delete_key(sub: &str) -> Result<(), String> {
    let parent_rel = match sub.rfind('\\') {
        Some(i) => &sub[..i],
        None => return Ok(()),
    };
    let name = &sub[sub.rfind('\\').map(|i| i + 1).unwrap_or(0)..];
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.open_subkey_with_flags(parent_rel, KEY_READ | KEY_WRITE) {
        Ok(parent) => {
            let _ = parent.delete_subkey(name); // 不存在 → Ok
            Ok(())
        }
        Err(_) => Ok(()), // 父键不存在 → 子键必然不存在
    }
}

/// 通知资源管理器文件关联已变更（立即生效，无需重启 explorer）
fn notify_assoc_changed() {
    // SHChangeNotify(SHCNE_ASSOCCHANGED=0x08000000, SHCNF_IDLIST=0, NULL, NULL)
    extern "system" {
        fn SHChangeNotify(
            wEventId: usize,
            uFlags: u32,
            dwItem1: *const core::ffi::c_void,
            dwItem2: *const core::ffi::c_void,
        );
    }
    unsafe {
        SHChangeNotify(0x0800_0000, 0, core::ptr::null(), core::ptr::null());
    }
}

// ==================== 注册 / 注销 ====================

/// 写 ProgID 树（类型描述 + 图标 + open 命令；幂等覆盖）
fn write_progid(prog_id: &str, desc: &str, exe: &str) -> Result<(), String> {
    let prog_key = format!(r"Software\Classes\{}", prog_id);
    let k = open_or_create(&prog_key)?;
    k.set_value("", &desc)
        .map_err(|e| format!("写 ProgID {prog_id} 失败: {e}"))?;
    let icon = open_or_create(&format!(r"{}\DefaultIcon", prog_key))?;
    icon.set_value("", &format!("{exe},0"))
        .map_err(|e| format!("写图标失败: {e}"))?;
    let cmd = open_or_create(&format!(r"{}\shell\open\command", prog_key))?;
    cmd.set_value("", &format!("{exe} \"%1\""))
        .map_err(|e| format!("写 open 命令失败: {e}"))?;
    Ok(())
}

/// 扩展名列表 → OpenWithProgids + Capabilities FileAssociations
fn write_exts(exts: &[&str], prog_id: &str) -> Result<(), String> {
    for ext in exts {
        let k = open_or_create(&format!(r"Software\Classes\.{}\OpenWithProgids", ext))?;
        // 值数据为空（系统约定 OpenWithProgids 成员为空 REG_SZ）
        k.set_value(prog_id, &"")
            .map_err(|e| format!("关联 .{ext} 失败: {e}"))?;
        let fa = open_or_create(&format!(r"{}\FileAssociations", CAPABILITIES_KEY))?;
        fa.set_value(&format!(".{ext}"), &prog_id)
            .map_err(|e| format!("写 Capabilities .{ext} 失败: {e}"))?;
    }
    Ok(())
}

/// 注册文件关联 + 右键菜单（幂等：重复调用覆盖写）
pub fn register() -> Result<(), String> {
    let exe = exe_quoted();

    // 1. ProgID：图片（看图窗口）/ 视频（独立播放器）
    write_progid(PROG_ID, "jietu-hdr 图片", &exe)?;
    write_progid(VIDEO_PROG_ID, "jietu-hdr 视频", &exe)?;

    // 2. 每扩展名 OpenWithProgids（「打开方式」候选）+ Capabilities（默认应用页）
    write_exts(ASSOC_EXTS, PROG_ID)?;
    write_exts(VIDEO_ASSOC_EXTS, VIDEO_PROG_ID)?;

    // 3. RegisteredApplications + Capabilities（Win11 设置·默认应用可见）
    let reg_apps = open_or_create(r"Software\RegisteredApplications")?;
    reg_apps
        .set_value(APP_NAME, &CAPABILITIES_KEY)
        .map_err(|e| format!("登记 RegisteredApplications 失败: {}", e))?;
    let caps = open_or_create(CAPABILITIES_KEY)?;
    caps.set_value("ApplicationName", &"jietu-hdr")
        .map_err(|e| format!("写 ApplicationName 失败: {}", e))?;
    caps.set_value(
        "ApplicationDescription",
        &"温柔治愈系 HDR 截图 · 看图 · 视频录制与播放 · 内存清理工具",
    )
    .map_err(|e| format!("写 ApplicationDescription 失败: {}", e))?;

    // 4. 右键 verb（图片 / PDF / 视频 / 目录 / 目录背景）
    write_verb(
        VERB_IMAGE_OPEN,
        "用 jietu-hdr 打开",
        &format!("{} \"%1\"", exe),
    )?;
    write_verb(
        VERB_PDF_OPEN,
        "用 jietu-hdr 打开",
        &format!("{} \"%1\"", exe),
    )?;
    write_verb(
        VERB_IMAGE_VAULT,
        "添加到 jietu 隐私相册",
        &format!("{} --vault-add \"%1\"", exe),
    )?;
    write_verb(
        VERB_VIDEO_PLAY,
        "用 jietu-hdr 播放",
        &format!("{} \"%1\"", exe),
    )?;
    write_verb(
        VERB_DIR_BROWSE,
        "用 jietu-hdr 浏览此文件夹",
        &format!("{} --library \"%1\"", exe),
    )?;
    // 目录内空白处：占位符 %V 表示当前目录
    write_verb(
        VERB_DIR_BG_BROWSE,
        "用 jietu-hdr 浏览",
        &format!("{} --library \"%V\"", exe),
    )?;

    notify_assoc_changed();
    log::info!(
        "文件关联 + 右键菜单已注册（{} 图片 + {} 视频扩展名）",
        ASSOC_EXTS.len(),
        VIDEO_ASSOC_EXTS.len()
    );
    Ok(())
}

/// 写单个右键 verb：MUIVerb（菜单文案）+ Icon + command
fn write_verb(path: &str, verb: &str, command: &str) -> Result<(), String> {
    let exe = exe_quoted();
    let k = open_or_create(&format!(r"Software\Classes\{}", path))?;
    k.set_value("MUIVerb", &verb)
        .map_err(|e| format!("写 MUIVerb 失败: {}", e))?;
    k.set_value("Icon", &format!("{},0", exe))
        .map_err(|e| format!("写 Icon 失败: {}", e))?;
    let c = open_or_create(&format!(r"Software\Classes\{}\command", path))?;
    c.set_value("", &command)
        .map_err(|e| format!("写 verb command 失败: {}", e))?;
    Ok(())
}

/// 注销文件关联 + 右键菜单（不存在项静默跳过）
pub fn unregister() -> Result<(), String> {
    // ProgID 整树删除
    delete_key(&format!(r"Software\Classes\{}", PROG_ID))?;
    delete_key(&format!(r"Software\Classes\{}", VIDEO_PROG_ID))?;
    // 扩展名 OpenWithProgids 下的成员值
    for (exts, prog_id) in [(ASSOC_EXTS, PROG_ID), (VIDEO_ASSOC_EXTS, VIDEO_PROG_ID)] {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        for ext in exts {
            let sub = format!(r"Software\Classes\.{}\OpenWithProgids", ext);
            if let Ok(k) = hkcu.open_subkey_with_flags(&sub, KEY_READ | KEY_WRITE) {
                let _ = k.delete_value(prog_id);
            }
        }
    }
    // RegisteredApplications 登记项 + Capabilities 树
    if let Ok(k) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\RegisteredApplications", KEY_READ | KEY_WRITE)
    {
        let _ = k.delete_value(APP_NAME);
    }
    delete_key(r"Software\jietu-hdr")?;
    // 右键 verb
    delete_key(&format!(r"Software\Classes\{}", VERB_IMAGE_OPEN))?;
    delete_key(&format!(r"Software\Classes\{}", VERB_PDF_OPEN))?;
    delete_key(&format!(r"Software\Classes\{}", VERB_IMAGE_VAULT))?;
    delete_key(&format!(r"Software\Classes\{}", VERB_VIDEO_PLAY))?;
    delete_key(&format!(r"Software\Classes\{}", VERB_DIR_BROWSE))?;
    delete_key(&format!(r"Software\Classes\{}", VERB_DIR_BG_BROWSE))?;

    notify_assoc_changed();
    log::info!("文件关联 + 右键菜单已注销（含视频）");
    Ok(())
}

/// 查询注册状态（以 RegisteredApplications 登记项为准）
pub fn is_registered() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\RegisteredApplications", KEY_READ)
        .ok()
        .and_then(|k| k.get_value::<String, _>(APP_NAME).ok())
        .is_some()
}

// ==================== 设为默认看图应用 ====================
//
// Windows 8 起 UserChoice 写入受系统哈希保护（防应用劫持默认程序），
// 程序无法静默改默认；合规路径是调起系统确认界面：
//   IApplicationAssociationRegistrationUI::LaunchAdvancedAssociationUI("jietu-hdr")
//   → 弹出本应用专属关联页（含 Capabilities 登记的图片+视频全部格式），用户确认一次全设。
// 失败降级：ShellExecuteW "ms-settings:defaultapps" 深链。
// UserChoice 本身可读 → default_assoc_status 回显「已设默认 X/N 个格式」。

/// (扩展名, 对应 ProgID) 全表——图片 + 视频（状态查询统一入口）
fn all_exts() -> Vec<(&'static str, &'static str)> {
    ASSOC_EXTS
        .iter()
        .map(|e| (*e, PROG_ID))
        .chain(VIDEO_ASSOC_EXTS.iter().map(|e| (*e, VIDEO_PROG_ID)))
        .collect()
}

/// 读取各扩展名 UserChoice，统计已把本应用设为默认的数量 → (已设, 总数)
pub fn default_assoc_status() -> (usize, usize) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exts = all_exts();
    let mut set = 0;
    for (ext, prog_id) in &exts {
        let sub = format!(
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{}\UserChoice",
            ext
        );
        let ok = hkcu
            .open_subkey_with_flags(&sub, KEY_READ)
            .ok()
            .and_then(|k| k.get_value::<String, _>("ProgID").ok())
            .map(|v| v == *prog_id)
            .unwrap_or(false);
        if ok {
            set += 1;
        }
    }
    (set, exts.len())
}

/// 单扩展名关联状态（前端 interface AssocExtStatus）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssocExtStatus {
    /// 扩展名（小写，无点）
    pub ext: String,
    /// 默认处理方是否为本应用
    pub ours: bool,
    /// 当前默认处理方可读名（如 "jietu-hdr 图片" / "2345看图王"）；无默认为 None
    pub owner: Option<String>,
    /// 当前 UserChoice ProgID（诊断用；无默认为 None）
    pub prog_id: Option<String>,
}

/// 逐扩展名查询默认关联状态（UserChoice 只读）
///
/// owner 可读名解析：本应用 ProgID 直判；其余查 Classes 根键的默认值
/// （ProgID 友好名，如 2345Pic.bmp → "2345看图王"），AppX* 显示"商店应用"。
pub fn assoc_detail() -> Vec<AssocExtStatus> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exts = all_exts();
    let mut out = Vec::with_capacity(exts.len());
    for (ext, ours_pid) in exts {
        let sub = format!(
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{}\UserChoice",
            ext
        );
        let prog_id: Option<String> = hkcu
            .open_subkey_with_flags(&sub, KEY_READ)
            .ok()
            .and_then(|k| k.get_value::<String, _>("ProgID").ok());
        let ours = prog_id.as_deref() == Some(ours_pid);
        let owner = prog_id.as_deref().map(|pid| owner_name(pid));
        out.push(AssocExtStatus {
            ext: ext.to_string(),
            ours,
            owner,
            prog_id,
        });
    }
    out
}

/// ProgID → 处理方可读名
fn owner_name(pid: &str) -> String {
    if pid == PROG_ID {
        return "jietu-hdr".to_string();
    }
    if pid == VIDEO_PROG_ID {
        return "jietu-hdr 视频".to_string();
    }
    if pid.starts_with("AppX") {
        return "商店应用".to_string();
    }
    // ProgID 根键默认值（应用登记的友好名）；去掉常见 "Applications\xxx.exe" 前缀
    let clean = pid.strip_prefix(r"Applications\").unwrap_or(pid);
    for root in [
        RegKey::predef(HKEY_CURRENT_USER),
        RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE),
    ] {
        if let Ok(name) = root
            .open_subkey_with_flags(&format!(r"Software\Classes\{}", pid), KEY_READ)
            .and_then(|k| k.get_value::<String, _>(""))
        {
            if !name.trim().is_empty() {
                return name;
            }
        }
    }
    clean.to_string()
}

/// COM GUID（raw FFI，风格对齐 set_wallpaper）
#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// CLSID_ApplicationAssociationRegistrationUI
const CLSID_AAR_UI: Guid = Guid {
    data1: 0x5912_09c7,
    data2: 0x767b,
    data3: 0x42b2,
    data4: [0x9f, 0xba, 0x44, 0xee, 0x46, 0x15, 0xf2, 0xc7],
};
/// IID_IApplicationAssociationRegistrationUI
const IID_IAR_UI: Guid = Guid {
    data1: 0x4b52_8c47,
    data2: 0x162f,
    data3: 0x4a75,
    data4: [0x89, 0x71, 0xa2, 0x78, 0x31, 0x37, 0x0f, 0x86],
};

type VoidPtr = *mut core::ffi::c_void;

/// IApplicationAssociationRegistrationUI 虚表（IUnknown 3 槽 + Launch 槽 3）
#[repr(C)]
struct IarUiVtable {
    _query_interface: *const core::ffi::c_void,
    _add_ref: *const core::ffi::c_void,
    release: unsafe extern "system" fn(VoidPtr) -> u32,
    launch_advanced_association_ui: unsafe extern "system" fn(VoidPtr, *const u16) -> i32,
}

#[repr(C)]
struct IarUi {
    vtable: *const IarUiVtable,
}

/// 打开系统「设置关联」确认界面（本应用专属页，用户确认一次设全部格式）
///
/// 模态调用（阻塞至用户关闭界面），须在线程池执行；失败降级 ms-settings 深链。
pub fn launch_default_apps_ui() -> Result<(), String> {
    // 关联页需要 Capabilities 登记条目，先确保已注册
    register()?;

    extern "system" {
        fn CoInitializeEx(pv: VoidPtr, dw_co_init: u32) -> i32;
        fn CoUninitialize();
        fn CoCreateInstance(
            rclsid: *const Guid,
            punk_outer: VoidPtr,
            dw_cls_context: u32,
            riid: *const Guid,
            ppv: *mut VoidPtr,
        ) -> i32;
        fn ShellExecuteW(
            hwnd: VoidPtr,
            verb: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> isize;
    }
    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const SW_SHOWNORMAL: i32 = 1;

    unsafe {
        let hr_init = CoInitializeEx(core::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        if hr_init < 0 {
            log::warn!(
                "CoInitializeEx 失败 0x{:08X}，降级 ms-settings",
                hr_init as u32
            );
            return open_ms_settings(ShellExecuteW, SW_SHOWNORMAL);
        }
        let mut ui: VoidPtr = core::ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_AAR_UI,
            core::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IAR_UI,
            &mut ui,
        );
        if hr == 0 && !ui.is_null() {
            let iface = &*(ui as *const IarUi);
            let vt = &*iface.vtable;
            let wide: Vec<u16> = APP_NAME.encode_utf16().chain(core::iter::once(0)).collect();
            let hr_launch = (vt.launch_advanced_association_ui)(ui, wide.as_ptr());
            (vt.release)(ui);
            CoUninitialize();
            if hr_launch == 0 {
                log::info!("已打开系统关联确认界面（LaunchAdvancedAssociationUI）");
                return Ok(());
            }
            log::warn!(
                "LaunchAdvancedAssociationUI 返回 0x{:08X}，降级 ms-settings",
                hr_launch as u32
            );
            return open_ms_settings(ShellExecuteW, SW_SHOWNORMAL);
        }
        log::warn!(
            "CoCreateInstance 关联 UI 失败 0x{:08X}，降级 ms-settings",
            hr as u32
        );
        CoUninitialize();
        open_ms_settings(ShellExecuteW, SW_SHOWNORMAL)
    }
}

/// 降级：ShellExecuteW 打开 Win10/11 设置 · 默认应用页
unsafe fn open_ms_settings(
    shell_execute: unsafe extern "system" fn(
        VoidPtr,
        *const u16,
        *const u16,
        *const u16,
        *const u16,
        i32,
    ) -> isize,
    sw_shownormal: i32,
) -> Result<(), String> {
    let file: Vec<u16> = "ms-settings:defaultapps"
        .encode_utf16()
        .chain(core::iter::once(0))
        .collect();
    let rc = shell_execute(
        core::ptr::null_mut(),
        core::ptr::null(),
        file.as_ptr(),
        core::ptr::null(),
        core::ptr::null(),
        sw_shownormal,
    );
    if rc > 32 {
        log::info!("已打开 ms-settings:defaultapps");
        Ok(())
    } else {
        Err(format!("打开系统设置失败（ShellExecuteW 返回 {}）", rc))
    }
}
