//! 内存清理模块（行为等价复刻 Mem Reduct，GPL-3.0 合规策略见 docs/viewer-design.md 4.1.5）
//!
//! 实现：8 区域 NT 清理（API 调用序列逐行对照 memreduct src/main.c 标注溯源）、
//! 全字段内存信息（GlobalMemoryStatusEx + NT 查询）、1 秒调度器（阈值/间隔/冷却自动清理，
//! 仅提权态生效）、托盘百分比徽章（GDI 绘制 + 防闪烁缓存）、权限判定与提权重启。
//!
//! 注意：NtSetSystemInformation / NtQuerySystemInformation / RtlGetVersion 为 ntdll
//! 未导出文档 API，windows crate 0.58 不提供，故用 GetModuleHandleW + GetProcAddress
//! 动态获取（对照设计 4.2.1 nt_ffi 方案）。

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, COLORREF, HANDLE, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, FillRect, SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
    DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_BOLD, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, TokenElevation,
    LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, FILE_ATTRIBUTE_NORMAL, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_EXISTING,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    SetProcessWorkingSetSizeEx, SETPROCESSWORKINGSETSIZEEX_FLAGS,
};
use windows::Win32::System::SystemInformation::{
    GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::config::{CleanStats, Config};

// ==================== 清理区域 mask（对照 main.h:L42-L62） ====================

/// 工作集（REDUCT_WORKINGSET）
pub const WORKINGSET: u32 = 0x01;
/// 系统文件缓存（REDUCT_SYSTEMFILECACHE）
pub const SYSTEMFILECACHE: u32 = 0x02;
/// 低优先级备用列表（REDUCT_STANDBYPRIORITY0LIST）
pub const STANDBYPRIORITY0LIST: u32 = 0x04;
/// 备用列表（REDUCT_STANDBYLIST）—— 危险区域
pub const STANDBYLIST: u32 = 0x08;
/// 已修改页列表（REDUCT_MODIFIEDLIST）—— 危险区域
pub const MODIFIEDLIST: u32 = 0x10;
/// 合并物理内存页（REDUCT_COMBINEMORYLISTS，Win10+）
pub const COMBINEMEMORYLISTS: u32 = 0x20;
/// 注册表缓存（REDUCT_REGISTRYCACHE，Win8.1+）
pub const REGISTRYCACHE: u32 = 0x40;
/// 卷修改缓存（REDUCT_MODIFIEDFILECACHE）
pub const MODIFIEDFILECACHE: u32 = 0x80;

/// 默认组合 = 0x01|0x02|0x04|0x20|0x40|0x80 = 231（对照 main.h:L61，不含两个危险区域）
pub const MASK_DEFAULT: u32 = WORKINGSET
    | SYSTEMFILECACHE
    | STANDBYPRIORITY0LIST
    | COMBINEMEMORYLISTS
    | REGISTRYCACHE
    | MODIFIEDFILECACHE;
/// 危险组合 = 备用列表 | 已修改页列表（对照 main.h:L62，UI 需二次确认）
pub const MASK_FREEZES: u32 = STANDBYLIST | MODIFIEDLIST;

// ==================== NT 常量（数值以 phnt 头文件核对） ====================

/// SYSTEM_INFORMATION_CLASS::SystemPerformanceInformation = 2
const CLASS_SYSTEM_PERFORMANCE_INFORMATION: u32 = 2;
/// SYSTEM_INFORMATION_CLASS::SystemMemoryListInformation = 80（0x50）
/// 长度 4 = 写入命令（SET），大缓冲 = 查询内存列表（QUERY）
const CLASS_SYSTEM_MEMORY_LIST_INFORMATION: u32 = 80;
/// SYSTEM_INFORMATION_CLASS::SystemFileCacheInformationEx = 81（0x51）
const CLASS_SYSTEM_FILE_CACHE_INFORMATION_EX: u32 = 81;
/// SYSTEM_INFORMATION_CLASS::SystemCombinePhysicalMemoryInformation = 130（0x82）
const CLASS_SYSTEM_COMBINE_PHYSICAL_MEMORY_INFORMATION: u32 = 130;
/// SYSTEM_INFORMATION_CLASS::SystemRegistryReconciliationInformation = 155（0x9B）
const CLASS_SYSTEM_REGISTRY_RECONCILIATION_INFORMATION: u32 = 155;

/// SYSTEM_MEMORY_LIST_COMMAND（phnt）：MemoryEmptyWorkingSets = 2
const CMD_MEMORY_EMPTY_WORKING_SETS: u32 = 2;
/// MemoryFlushModifiedList = 3
const CMD_MEMORY_FLUSH_MODIFIED_LIST: u32 = 3;
/// MemoryPurgeStandbyList = 4
const CMD_MEMORY_PURGE_STANDBY_LIST: u32 = 4;
/// MemoryPurgeLowPriorityStandbyList = 5
const CMD_MEMORY_PURGE_LOW_PRIORITY_STANDBY_LIST: u32 = 5;

/// IOCTL_MOUNTMGR_QUERY_POINTS = CTL_CODE(0x6D, 2, METHOD_BUFFERED, FILE_ANY_ACCESS)（mountmgr.h）
const IOCTL_MOUNTMGR_QUERY_POINTS: u32 = 0x006D0008;

// ==================== NT FFI 动态加载 ====================

/// NtSetSystemInformation 函数签名（NTSTATUS >= 0 为成功）
type NtSetSystemInformationFn =
    unsafe extern "system" fn(class: u32, info: *mut c_void, len: u32) -> i32;
/// NtQuerySystemInformation 函数签名
type NtQuerySystemInformationFn =
    unsafe extern "system" fn(class: u32, info: *mut c_void, len: u32, ret_len: *mut u32) -> i32;
/// RtlGetVersion 函数签名（不受兼容性垫片影响，返回真实版本号）
type RtlGetVersionFn = unsafe extern "system" fn(info: *mut RtlOsVersionInfoW) -> i32;

/// RTL_OSVERSIONINFOW（RtlGetVersion 输出）
#[repr(C)]
struct RtlOsVersionInfoW {
    dw_os_version_info_size: u32,
    dw_major_version: u32,
    dw_minor_version: u32,
    dw_build_number: u32,
    dw_platform_id: u32,
    sz_csd_version: [u16; 128],
}

/// ntdll 动态解析出的 NT API 集合（进程内只解析一次）
struct NtApi {
    set_system_information: NtSetSystemInformationFn,
    query_system_information: NtQuerySystemInformationFn,
    rtl_get_version: RtlGetVersionFn,
}

static NT_API: OnceLock<Option<NtApi>> = OnceLock::new();

/// 获取 NT API（首次调用时从 ntdll.dll 动态解析，失败返回 None）
fn nt_api() -> Option<&'static NtApi> {
    NT_API
        .get_or_init(|| unsafe {
            let h = GetModuleHandleW(w!("ntdll.dll")).ok()?;
            // FARPROC = Option<fn 指针>（8 字节），逐个判 Some 后 transmute 为目标签名
            let set = GetProcAddress(h, s!("NtSetSystemInformation"));
            let query = GetProcAddress(h, s!("NtQuerySystemInformation"));
            let ver = GetProcAddress(h, s!("RtlGetVersion"));
            if set.is_none() || query.is_none() || ver.is_none() {
                log::error!("ntdll NT API 解析失败，内存清理 NT 调用不可用");
                return None;
            }
            Some(NtApi {
                set_system_information: std::mem::transmute(set),
                query_system_information: std::mem::transmute(query),
                rtl_get_version: std::mem::transmute(ver),
            })
        })
        .as_ref()
}

/// NtSetSystemInformation 调用，返回 NTSTATUS（>=0 成功）
unsafe fn nt_set(class: u32, info: *mut c_void, len: u32) -> i32 {
    match nt_api() {
        Some(api) => (api.set_system_information)(class, info, len),
        None => 0xC0000135u32 as i32, // STATUS_DLL_NOT_FOUND（ntdll 解析失败）
    }
}

/// NtQuerySystemInformation 调用，返回 NTSTATUS（>=0 成功，结果写入 info 缓冲）
unsafe fn nt_query(class: u32, info: *mut c_void, len: u32, ret_len: *mut u32) -> i32 {
    match nt_api() {
        Some(api) => (api.query_system_information)(class, info, len, ret_len),
        None => 0xC0000135u32 as i32,
    }
}

/// NTSTATUS → 可读错误串（常见值映射中文，便于用户理解失败原因）
fn nt_status_str(status: i32) -> String {
    let u = status as u32;
    let desc = match u {
        0xC0000061 => "需要管理员权限（此清理区域要求系统级特权，请以管理员身份重启）",
        0xC0000022 => "访问被拒绝（请尝试以管理员身份运行）",
        0xC0000004 => "信息长度不匹配",
        0xC000000D => "参数无效（当前 Windows 版本可能不支持此区域）",
        0xC00000BB => "不支持的操作（当前 Windows 版本可能不支持此区域）",
        0xC0000350 => "特权未被持有（请以管理员身份重启）",
        0xC0000135 => "NT API 解析失败（ntdll 缺失，异常环境）",
        _ => return format!("NTSTATUS 0x{:08X}", u),
    };
    format!("NTSTATUS 0x{:08X}（{}）", u, desc)
}

/// 系统构建号 ≥ 指定值（RtlGetVersion 真实值，不受 manifest 兼容垫片影响）
/// Win8.1 = 9600、Win10 = 10240（对照 main.c 的 _r_sys_isosversiongreaterorequal 判定）
fn is_win_build_at_least(build: u32) -> bool {
    static BUILD: OnceLock<u32> = OnceLock::new();
    let build_number = *BUILD.get_or_init(|| unsafe {
        match nt_api() {
            Some(api) => {
                let mut info: RtlOsVersionInfoW = std::mem::zeroed();
                info.dw_os_version_info_size = std::mem::size_of::<RtlOsVersionInfoW>() as u32;
                if (api.rtl_get_version)(&mut info) >= 0 {
                    info.dw_build_number
                } else {
                    0
                }
            }
            None => 0,
        }
    });
    build_number >= build
}

/// 系统页大小（4KB，通常），进程内缓存一次
fn page_size() -> u64 {
    static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        si.dwPageSize as u64
    })
}

// ==================== NT 查询结构定义（x64 布局） ====================

/// SYSTEM_PERFORMANCE_INFORMATION 前段字段（phnt 布局；完整结构更长，缓冲开大后按前缀解释）
#[repr(C)]
struct SystemPerformanceInformation {
    _idle_process_time: i64,
    _io_read_transfer_count: i64,
    _io_write_transfer_count: i64,
    _io_other_transfer_count: i64,
    _io_read_operation_count: u32,
    _io_write_operation_count: u32,
    _io_other_operation_count: u32,
    _available_pages: u32,
    _committed_pages: u32,
    _commit_limit: u32,
    _peak_commitment: u32,
    _page_fault_count: u32,
    _copy_on_write_count: u32,
    _transition_count: u32,
    _cache_transition_count: u32,
    _demand_zero_count: u32,
    _page_read_count: u32,
    _page_read_io_count: u32,
    _cache_read_count: u32,
    _cache_io_count: u32,
    _dirty_pages_write_count: u32,
    _dirty_write_io_count: u32,
    _mapped_pages_write_count: u32,
    _mapped_write_io_count: u32,
    /// 页面池页数（× 页大小 = paged_pool 字节）
    paged_pool_pages: u32,
    /// 非页面池页数
    non_paged_pool_pages: u32,
    _paged_pool_allocs: u32,
    _paged_pool_frees: u32,
    _non_paged_pool_allocs: u32,
    _non_paged_pool_frees: u32,
    _free_system_ptes: u32,
    _resident_system_code_page: u32,
    _total_system_driver_pages: u32,
    _total_system_code_pages: u32,
    _non_paged_pool_lookaside_hits: u32,
    _paged_pool_lookaside_hits: u32,
    _available_paged_pool_pages: u32,
    /// 驻留系统缓存页数（× 页大小 = system_cache 字节）
    resident_system_cache_page: u32,
    _resident_paged_pool_page: u32,
    _resident_system_driver_page: u32,
}

/// SYSTEM_MEMORY_LIST_INFORMATION 查询结果（phnt；ULONG_PTR 字段）
#[repr(C)]
struct SystemMemoryListInformation {
    _zero_page_count: usize,
    _free_page_count: usize,
    /// 已修改页列表页数
    modified_page_count: usize,
    _modified_no_write_page_count: usize,
    _bad_page_count: usize,
    /// 备用列表按优先级 0-7 的页数（总和 = standby 字节）
    page_count_by_priority: [usize; 8],
    _repurposed_pages_by_priority: [usize; 8],
}

/// SYSTEM_FILECACHE_INFORMATION（phnt/NtDoc 全 9 字段布局，x64 sizeof=64）
///
/// 注意：内核按 sizeof 校验长度（传短了返回 STATUS_INFO_LENGTH_MISMATCH），
/// 故必须使用完整结构（对照 main.c:L405 传 sizeof(SYSTEM_FILECACHE_INFORMATION)）。
#[repr(C)]
struct SystemFileCacheInformation {
    _current_size: usize,
    _peak_size: usize,
    _page_fault_count: u32,
    _pad: u32, // x64 对齐填充（与 C 布局一致）
    minimum_working_set: usize,
    maximum_working_set: usize,
    _current_size_including_transition_in_pages: usize,
    _peak_size_including_transition_in_pages: usize,
    _transition_repurpose_count: u32,
    flags: u32,
}

/// MEMORY_COMBINE_INFORMATION_EX（phnt；全零传入对照 main.c:L276/L465）
#[repr(C)]
struct MemoryCombineInformationEx {
    handle: usize,
    pages_combined: usize,
    flags: u32,
}

/// MOUNTMGR_MOUNT_POINT（mountmgr.h；C 端为 3×(ULONG+USHORT) 自然对齐至 24 字节，
/// reserved 字段占据 C 布局的填充位，字段偏移与 WDK 头一致：0/4/8/12/16/20）
#[repr(C)]
struct MountMgrMountPoint {
    symbolic_link_name_offset: u32,
    symbolic_link_name_length: u16,
    _reserved1: u16,
    _unique_id_offset: u32,
    _unique_id_length: u16,
    _reserved2: u16,
    _device_name_offset: u32,
    _device_name_length: u16,
    _reserved3: u16,
}

// ==================== 数据结构 ====================

/// 单类内存用量（百分比 + 已用/总量字节）
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MemoryUsage {
    /// 占用百分比（0-100）
    pub percent: f32,
    /// 已用字节
    pub used: u64,
    /// 总量字节
    pub total: u64,
}

/// 全量内存信息（对照 main.c:L148-L155 _app_getmemoryinfo → _r_sys_getmemoryinfo 全字段）
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MemoryInfo {
    /// 物理内存（GlobalMemoryStatusEx）
    pub physical: MemoryUsage,
    /// 提交内存（ullTotalPageFile/ullAvailPageFile）
    pub commit: MemoryUsage,
    /// 虚拟内存（ullTotalVirtual/ullAvailVirtual）
    pub virtual_mem: MemoryUsage,
    /// 系统缓存字节（SystemPerformanceInformation.ResidentSystemCachePage）
    pub system_cache: u64,
    /// 备用列表字节（SystemMemoryListInformation 各优先级之和）
    pub standby: u64,
    /// 已修改页列表字节
    pub modified: u64,
    /// 页面池字节
    pub paged_pool: u64,
    /// 非页面池字节
    pub non_paged_pool: u64,
}

/// 单区域清理结果
#[derive(Clone, Debug, Serialize)]
pub struct RegionResult {
    /// 区域 mask 位
    pub mask: u32,
    /// 区城中文名
    pub name: String,
    /// 是否成功
    pub ok: bool,
    /// 失败时的错误信息（NTSTATUS 等）
    pub error: Option<String>,
}

/// 清理结果（对照 main.c:L474-L477 前后差值）
#[derive(Clone, Debug, Serialize)]
pub struct CleanResult {
    /// 释放字节数（before - after，负值归零）
    pub freed_bytes: u64,
    /// 逐区域执行结果
    pub per_region: Vec<RegionResult>,
}

/// 清理完成通知载荷（前端 toast「已释放 X · 来源」用）
#[derive(Clone, Debug, Serialize)]
pub struct CleanNotification {
    pub source: String,
    /// 来源中文名（手动/自动/热键/命令行，对照 main.c:L157-L188 四分类）
    pub source_label: String,
    pub freed_bytes: u64,
    pub per_region: Vec<RegionResult>,
}

/// 清理来源 → 中文名（对照 main.c:L157-L188 _app_getcleanupreason）
fn source_label(source: &str) -> &'static str {
    match source {
        "auto" => "自动清理",
        "hotkey" => "热键清理",
        "cmdline" => "命令行清理",
        _ => "手动清理",
    }
}

/// 字节数人性化（toast/日志用）
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0usize;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[unit])
    }
}

// ==================== 内存信息查询 ====================

/// 获取全量内存信息（对照 main.c:L148-L155）
///
/// - 物理/提交/虚拟：GlobalMemoryStatusEx（windows crate，Win32_System_SystemInformation）
/// - 系统缓存/页面池/非页面池：NtQuerySystemInformation(SystemPerformanceInformation)
/// - 备用列表/已修改页：NtQuerySystemInformation(SystemMemoryListInformation)（查询模式）
#[tauri::command]
pub fn get_memory_info() -> MemoryInfo {
    // GlobalMemoryStatusEx（对照 _r_sys_getmemoryinfo）
    let mut status = MEMORYSTATUSEX::default();
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ms_ok = unsafe { GlobalMemoryStatusEx(&mut status) }.is_ok();

    let (phys_total, phys_avail, phys_load, commit_total, commit_avail, virt_total, virt_avail) =
        if ms_ok {
            (
                status.ullTotalPhys,
                status.ullAvailPhys,
                status.dwMemoryLoad, // 物理占用百分比直接取系统值（对照 dwMemoryLoad）
                status.ullTotalPageFile,
                status.ullAvailPageFile,
                status.ullTotalVirtual,
                status.ullAvailVirtual,
            )
        } else {
            (0, 0, 0, 0, 0, 0, 0)
        };

    let usage_from = |total: u64, avail: u64, percent: Option<u32>| MemoryUsage {
        percent: match percent {
            Some(p) => p as f32,
            None => {
                if total > 0 {
                    ((total - avail) as f64 / total as f64 * 100.0) as f32
                } else {
                    0.0
                }
            }
        },
        used: total.saturating_sub(avail),
        total,
    };

    // NT 查询：系统缓存/页面池/非页面池（SystemPerformanceInformation）
    let (system_cache, paged_pool, non_paged_pool) = unsafe {
        let mut buf = [0u8; 1024];
        let mut ret_len = 0u32;
        let st = nt_query(
            CLASS_SYSTEM_PERFORMANCE_INFORMATION,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut ret_len,
        );
        if st < 0 {
            log::debug!(
                "SystemPerformanceInformation 查询失败: {}",
                nt_status_str(st)
            );
            (0, 0, 0)
        } else {
            let spi = &*(buf.as_ptr() as *const SystemPerformanceInformation);
            let ps = page_size();
            (
                spi.resident_system_cache_page as u64 * ps,
                spi.paged_pool_pages as u64 * ps,
                spi.non_paged_pool_pages as u64 * ps,
            )
        }
    };

    // NT 查询：备用列表（各优先级之和）/已修改页列表（SystemMemoryListInformation 查询模式）
    let (standby, modified) = unsafe {
        let mut buf = [0u8; 1024];
        let mut ret_len = 0u32;
        let st = nt_query(
            CLASS_SYSTEM_MEMORY_LIST_INFORMATION,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut ret_len,
        );
        if st < 0 {
            log::debug!(
                "SystemMemoryListInformation 查询失败: {}",
                nt_status_str(st)
            );
            (0, 0)
        } else {
            let mli = &*(buf.as_ptr() as *const SystemMemoryListInformation);
            let ps = page_size();
            let standby_pages: u64 = mli.page_count_by_priority.iter().map(|&c| c as u64).sum();
            (standby_pages * ps, mli.modified_page_count as u64 * ps)
        }
    };

    MemoryInfo {
        physical: usage_from(phys_total, phys_avail, Some(phys_load)),
        commit: usage_from(commit_total, commit_avail, None),
        virtual_mem: usage_from(virt_total, virt_avail, None),
        system_cache,
        standby,
        modified,
        paged_pool,
        non_paged_pool,
    }
}

// ==================== 8 区域清理（对照 main.c:L270-L509 _app_memoryclean） ====================

/// NtSetSystemInformation(SystemMemoryListInformation, command)（对照 main.c:L391-L393 等四处）
unsafe fn nt_memory_list_command(command: u32) -> Result<(), String> {
    let status = nt_set(
        CLASS_SYSTEM_MEMORY_LIST_INFORMATION,
        &command as *const u32 as *mut c_void,
        std::mem::size_of::<u32>() as u32,
    );
    if status >= 0 {
        Ok(())
    } else {
        Err(nt_status_str(status))
    }
}

/// 刷新卷修改缓存（对照 main.c:L190-L268 _app_flushvolumecache）
///
/// MountPointManager 设备枚举卷符号链接 → 逐卷 FILE_WRITE_DATA 打开 → FlushFileBuffers。
/// 原实现走 NtCreateFile/NtDeviceIoControlFile/NtFlushBuffersFile，此处用等价 Win32 薄封装。
fn flush_volume_cache() -> Result<(), String> {
    // DeviceIoControl 所属 feature（Win32_System_IO）未启用，按 lib.rs 的
    // SetWindowDisplayAffinity 先例用 extern "system" 声明
    extern "system" {
        fn DeviceIoControl(
            hdevice: *mut c_void,
            dwiocontrolcode: u32,
            lpinbuffer: *const c_void,
            ninbuffersize: u32,
            lpoutbuffer: *mut c_void,
            noutbuffersize: u32,
            lpbytesreturned: *mut u32,
            lpoverlapped: *mut c_void,
        ) -> i32;
    }

    unsafe {
        // 1. 打开 MountPointManager 设备（对照 main.c:L204-L216：FILE_READ_ATTRIBUTES|SYNCHRONIZE）
        let hdevice = CreateFileW(
            w!("\\\\.\\MountPointManager"),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            HANDLE::default(), // hTemplateFile 置空
        )
        .map_err(|e| format!("打开 MountPointManager 失败: {}", e))?;

        // 2. IOCTL_MOUNTMGR_QUERY_POINTS 枚举挂载点（对照 _r_fs_getvolumemountpoints）
        //    注意（WDK 文档）：输入必须 ≥ sizeof(MOUNTMGR_MOUNT_POINT)=24 字节；
        //    传入全零「空三元组」= 枚举全部挂载点（长度 0 会返回 STATUS_INVALID_PARAMETER=87）
        let input = [0u8; 24]; // 空的 MOUNTMGR_MOUNT_POINT 三元组
        let mut buffer_len: usize = 4096;
        let mount_points_bytes: Vec<u8> = loop {
            let mut buf = vec![0u8; buffer_len];
            let mut bytes_returned: u32 = 0;
            let ok = DeviceIoControl(
                hdevice.0,
                IOCTL_MOUNTMGR_QUERY_POINTS,
                input.as_ptr() as *const c_void,
                input.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
            );
            if ok != 0 {
                break buf[..bytes_returned as usize].to_vec();
            }
            let err = windows::Win32::Foundation::GetLastError().0;
            if err == 122 || err == 234 {
                // ERROR_INSUFFICIENT_BUFFER / ERROR_MORE_DATA：缓冲不足，翻倍重试（上限 1MB）
                buffer_len *= 2;
                if buffer_len > 1024 * 1024 {
                    let _ = CloseHandle(hdevice);
                    return Err("挂载点缓冲区需求超过 1MB，放弃".into());
                }
                continue;
            }
            let _ = CloseHandle(hdevice);
            return Err(format!(
                "IOCTL_MOUNTMGR_QUERY_POINTS 失败: Win32 错误 {}",
                err
            ));
        };
        let _ = CloseHandle(hdevice);

        // 3. 解析 MOUNTMGR_MOUNT_POINTS：{ ULONG Size; ULONG NumberOfMountPoints; MountPoints[] }
        if mount_points_bytes.len() < 8 {
            return Ok(());
        }
        let count = u32::from_le_bytes(mount_points_bytes[4..8].try_into().unwrap()) as usize;
        let base = mount_points_bytes.as_ptr();
        let entry_size = std::mem::size_of::<MountMgrMountPoint>();
        let mut flushed = 0usize;

        for i in 0..count {
            if 8 + (i + 1) * entry_size > mount_points_bytes.len() {
                break; // 缓冲截断保护
            }
            let mp = &*(base.add(8 + i * entry_size) as *const MountMgrMountPoint);
            let name_len = mp.symbolic_link_name_length as usize;
            if name_len == 0 {
                continue;
            }
            let name_off = mp.symbolic_link_name_offset as usize;
            if name_off + name_len > mount_points_bytes.len() || name_len % 2 != 0 {
                continue;
            }
            let name_utf16 =
                std::slice::from_raw_parts(base.add(name_off) as *const u16, name_len / 2);
            let name = String::from_utf16_lossy(name_utf16);

            // MOUNTMGR_IS_VOLUME_NAME 等价判定：仅处理 "\??\Volume{...}" 卷符号链接（对照 main.c:L234）
            if !name.starts_with("\\??\\Volume{") {
                continue;
            }
            // NT 路径前缀 "\??\" → Win32 "\\?\"
            let win32_path = format!("\\\\?\\{}", &name[4..]);
            let mut path_w: Vec<u16> = win32_path.encode_utf16().collect();
            path_w.push(0);

            // FILE_WRITE_DATA 打开卷（对照 main.c:L238-L250）
            let hvolume = CreateFileW(
                PCWSTR(path_w.as_ptr()),
                FILE_WRITE_DATA.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                HANDLE::default(), // hTemplateFile 置空
            );
            let Ok(hvolume) = hvolume else {
                continue; // 单卷失败不中断（对齐源码逐区域容错语义）
            };
            // 刷卷缓存（对照 main.c:L254 _r_fs_flushfile）
            if FlushFileBuffers(hvolume).is_ok() {
                flushed += 1;
            }
            let _ = CloseHandle(hvolume);
        }
        log::debug!("卷缓存刷新完成: {}/{} 个挂载点", flushed, count);
        Ok(())
    }
}

/// 更新清理统计并持久化（对照 main.c:L480 StatisticLastReduct；jietu 增加累计两项）
fn update_clean_stats(freed: u64) {
    let mut cfg = Config::load();
    let stats: &mut CleanStats = &mut cfg.memory_clean.stats;
    stats.last_clean_ts = chrono::Local::now().timestamp().max(0) as u64;
    stats.last_freed_bytes = freed;
    stats.total_clean_count += 1;
    stats.total_freed_bytes += freed;
    if let Err(e) = cfg.save() {
        log::warn!("清理统计保存失败: {}", e);
    }
}

/// 执行内存清理（对照 main.c:L270-L509 _app_memoryclean 完整流程）
///
/// - `mask`：区域组合位掩码；传 0 时使用配置默认（对照 main.c:L312-L313）
/// - `source`：manual / auto / hotkey / cmdline（对照 CLEANUP_SOURCE_ENUM）
/// - 单区域失败仅 log 不中断（对照源码逐区域独立 NT 调用 + _r_log 容错）
/// - 前后采样差值，负值归零（对照 main.c:L385-L386 / L474-L477）
pub fn perform_clean(mask: u32, source: &str) -> CleanResult {
    let cfg = Config::load();
    // 对照 main.c:L312-L313：mask 为 0 时取配置 mask（默认 REDUCT_MASK_DEFAULT=231）
    let mut mask = if mask == 0 {
        if cfg.memory_clean.mask == 0 {
            MASK_DEFAULT
        } else {
            cfg.memory_clean.mask
        }
    } else {
        mask
    };
    // 对照 main.c:L315-L319：自动清理排除危险区域（备用列表 + 已修改页列表）
    if source == "auto" {
        mask &= !MASK_FREEZES;
    }
    log::info!(
        "内存清理开始: 来源={} mask=0x{:02X}（提权态: {}）",
        source_label(source),
        mask,
        is_elevated()
    );

    let mut per_region: Vec<RegionResult> = Vec::new();
    let mut record = |bit: u32, name: &str, result: Result<(), String>| {
        // 失败仅记日志不中断（对照 main.c:L396 等处 _r_log LOG_LEVEL_ERROR）
        if let Err(ref e) = result {
            log::error!("内存清理区域失败 [{}]: {}", name, e);
        }
        per_region.push(RegionResult {
            mask: bit,
            name: name.to_string(),
            ok: result.is_ok(),
            error: result.err(),
        });
    };

    // 差值采样 before（对照 main.c:L385-L386）
    let before = get_memory_info().physical.used;

    // —— 区域执行顺序与 main.c:L388-L470 逐行对照 ——

    // 0x01 工作集（vista+，对照 main.c:L388-L397）：MemoryEmptyWorkingSets
    if mask & WORKINGSET != 0 {
        let r = unsafe { nt_memory_list_command(CMD_MEMORY_EMPTY_WORKING_SETS) };
        record(WORKINGSET, "工作集", r);
    }

    // 0x02 系统文件缓存（对照 main.c:L399-L409）：Min=Max=MAXSIZE_T
    if mask & SYSTEMFILECACHE != 0 {
        let r = unsafe {
            let mut sfci: SystemFileCacheInformation = std::mem::zeroed();
            sfci.minimum_working_set = usize::MAX; // MAXSIZE_T（对照 main.c:L402-L403）
            sfci.maximum_working_set = usize::MAX;
            let status = nt_set(
                CLASS_SYSTEM_FILE_CACHE_INFORMATION_EX,
                &mut sfci as *mut _ as *mut c_void,
                std::mem::size_of::<SystemFileCacheInformation>() as u32,
            );
            if status >= 0 {
                Ok(())
            } else {
                Err(nt_status_str(status))
            }
        };
        record(SYSTEMFILECACHE, "系统文件缓存", r);
    }

    // 0x80 卷修改缓存（对照 main.c:L411-L413）：_app_flushvolumecache
    if mask & MODIFIEDFILECACHE != 0 {
        record(MODIFIEDFILECACHE, "卷修改缓存", flush_volume_cache());
    }

    // 0x10 已修改页列表（vista+，对照 main.c:L415-L424）：MemoryFlushModifiedList —— 危险区域
    if mask & MODIFIEDLIST != 0 {
        let r = unsafe { nt_memory_list_command(CMD_MEMORY_FLUSH_MODIFIED_LIST) };
        record(MODIFIEDLIST, "已修改页列表*", r);
    }

    // 0x08 备用列表（vista+，对照 main.c:L426-L435）：MemoryPurgeStandbyList —— 危险区域
    if mask & STANDBYLIST != 0 {
        let r = unsafe { nt_memory_list_command(CMD_MEMORY_PURGE_STANDBY_LIST) };
        record(STANDBYLIST, "备用列表*", r);
    }

    // 0x04 低优先级备用列表（vista+，对照 main.c:L437-L446）：MemoryPurgeLowPriorityStandbyList
    if mask & STANDBYPRIORITY0LIST != 0 {
        let r = unsafe { nt_memory_list_command(CMD_MEMORY_PURGE_LOW_PRIORITY_STANDBY_LIST) };
        record(STANDBYPRIORITY0LIST, "低优先级备用列表", r);
    }

    // 0x40 注册表缓存（win8.1+，对照 main.c:L448-L458）：SystemRegistryReconciliationInformation(NULL, 0)
    if is_win_build_at_least(9600) {
        if mask & REGISTRYCACHE != 0 {
            let r = unsafe {
                let status = nt_set(
                    CLASS_SYSTEM_REGISTRY_RECONCILIATION_INFORMATION,
                    std::ptr::null_mut(),
                    0,
                );
                if status >= 0 {
                    Ok(())
                } else {
                    Err(nt_status_str(status))
                }
            };
            record(REGISTRYCACHE, "注册表缓存 (Win8.1+)", r);
        }
    }

    // 0x20 合并物理内存页（win10+，对照 main.c:L460-L470）：SystemCombinePhysicalMemoryInformation
    if is_win_build_at_least(10240) {
        if mask & COMBINEMEMORYLISTS != 0 {
            let r = unsafe {
                // 对照 main.c:L276：MEMORY_COMBINE_INFORMATION_EX combine_info_ex = {0}
                let mut info = MemoryCombineInformationEx {
                    handle: 0,
                    pages_combined: 0,
                    flags: 0,
                };
                let status = nt_set(
                    CLASS_SYSTEM_COMBINE_PHYSICAL_MEMORY_INFORMATION,
                    &mut info as *mut _ as *mut c_void,
                    std::mem::size_of::<MemoryCombineInformationEx>() as u32,
                );
                if status >= 0 {
                    Ok(())
                } else {
                    Err(nt_status_str(status))
                }
            };
            record(COMBINEMEMORYLISTS, "合并物理内存页 (Win10+)", r);
        }
    }

    // 差值采样 after（对照 main.c:L474-L477）：负值归零
    let after = get_memory_info().physical.used;
    let freed_bytes = if after < before { before - after } else { 0 };

    // 更新统计（对照 main.c:L480：StatisticLastReduct = now）
    update_clean_stats(freed_bytes);

    log::info!(
        "内存清理完成: 来源={} 释放 {}（{} 个区域）",
        source_label(source),
        format_bytes(freed_bytes),
        per_region.iter().filter(|r| r.ok).count()
    );

    CleanResult {
        freed_bytes,
        per_region,
    }
}

/// 清理完成通知（对照 main.c:L488-L504：BalloonCleanResults → jietu 事件推前端 toast）
/// + 结果写日志（对照 main.c:L506-L507：LogCleanResults）
pub fn emit_clean_notification(app: &AppHandle, result: &CleanResult, source: &str) {
    let cfg = Config::load();
    let notification = CleanNotification {
        source: source.to_string(),
        source_label: source_label(source).to_string(),
        freed_bytes: result.freed_bytes,
        per_region: result.per_region.clone(),
    };
    if cfg.memory_clean.notify_enable {
        if let Err(e) = app.emit("memory://cleaned", &notification) {
            log::warn!("清理通知事件发送失败: {}", e);
        }
    }
    if cfg.memory_clean.log_results {
        log::info!(
            "[{}] 清理结果: 释放 {}",
            source_label(source),
            format_bytes(result.freed_bytes)
        );
    }
}

// ==================== Tauri 命令 ====================

/// 执行内存清理（前端主按钮 / 危险项确认对话框后调用）
///
/// 清理为同步 NT 调用，包 spawn_blocking 避免阻塞 async 运行时（设计 4.2.1 线程模型）。
#[tauri::command]
pub async fn clean_memory(
    app: AppHandle,
    mask: u32,
    source: String,
) -> Result<CleanResult, String> {
    let src = source.clone();
    let result = tauri::async_runtime::spawn_blocking(move || perform_clean(mask, &src))
        .await
        .map_err(|e| format!("清理任务执行失败: {}", e))?;
    emit_clean_notification(&app, &result, &source);
    Ok(result)
}

/// 查询当前进程是否以管理员权限运行（对照 _r_sys_iselevated；token TokenElevation）
#[tauri::command]
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 提权态启用清理所需权限（对照 main.c:L1700-L1707 _app_initialize：
/// SeProfileSingleProcessPrivilege + SeIncreaseQuotaPrivilege）
fn enable_cleanup_privileges() -> Result<(), String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| format!("OpenProcessToken 失败: {}", e))?;

        for name in [
            w!("SeProfileSingleProcessPrivilege"),
            w!("SeIncreaseQuotaPrivilege"),
        ] {
            let mut luid = windows::Win32::Foundation::LUID::default();
            // lpsystemname=NULL 表示本机（PCWSTR::null() 等价 NULL）
            if let Err(e) = LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid) {
                log::warn!("LookupPrivilegeValueW 失败: {}", e);
                continue;
            }
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            if let Err(e) =
                AdjustTokenPrivileges(token, false, Some(&tp as *const _), 0, None, None)
            {
                log::warn!("AdjustTokenPrivileges 失败: {}", e);
            }
        }
        let _ = CloseHandle(token);
    }
    Ok(())
}

/// 以管理员身份重启自身（ShellExecuteW "runas"，对照设计 4.2.1 权限模型 / 2345Pic 提权思路）
///
/// 成功发起提权实例后退出当前实例（避免双托盘/双调度器）。
#[tauri::command]
pub fn restart_elevated(app: AppHandle) -> Result<(), String> {
    // 退出保护：重启前先同步完成录制收尾（有活跃会话/编码中时等待保存）。
    // 必须在 ShellExecuteW 之前——新实例只等旧实例释放单实例 mutex 10s
    // （singleinstance.rs），收尾若拖到 app.exit 前才做，延迟编码可达 ~1-2
    // 分钟会超过该窗口导致提权接管失败。UAC 被拒时录制已停止并保存（数据
    // 安全优先于会话延续）。
    crate::record::commands::flush_on_exit(&app);

    // ShellExecuteW 所属 feature（Win32_UI_Shell）未启用，按 lib.rs 先例 extern 声明
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            lpoperation: *const u16,
            lpfile: *const u16,
            lpparameters: *const u16,
            lpdirectory: *const u16,
            nshowcmd: i32,
        ) -> isize; // HINSTANCE，>32 为成功
    }

    let exe = std::env::current_exe().map_err(|e| format!("获取自身路径失败: {}", e))?;
    use std::os::windows::ffi::OsStrExt;
    let mut exe_w: Vec<u16> = exe.as_os_str().encode_wide().collect();
    exe_w.push(0);

    let verb = w!("runas");
    // --elevated-restart 标记：新实例等待旧实例退出释放单实例 mutex 后接管为首实例
    //（否则新实例会把自己判定为"第二实例"转发后退出，提权永远失败——其他 Win11 机器的失败点）
    let params = w!("--elevated-restart");
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe_w.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    if result > 32 {
        // 稍等 UAC 确认后新实例拉起，再退出旧实例（新实例会等本进程释放 mutex）
        std::thread::sleep(std::time::Duration::from_millis(300));
        log::info!("提权重启已发起，退出当前普通权限实例");
        app.exit(0);
        Ok(())
    } else {
        Err(format!(
            "提权重启失败（可能拒绝了 UAC 提示），ShellExecuteW 返回 {}",
            result
        ))
    }
}

/// 清理自身工作集（SetProcessWorkingSetSize(-1,-1) 语义，无需管理员）
///
/// windows crate 0.58 仅导出 Ex 变体，flags=0 等价原 API。
#[tauri::command]
pub fn clean_own_working_set() -> Result<(), String> {
    unsafe {
        SetProcessWorkingSetSizeEx(
            GetCurrentProcess(),
            usize::MAX, // -1（清空下限）
            usize::MAX, // -1（清空上限）
            SETPROCESSWORKINGSETSIZEEX_FLAGS(0),
        )
        .map_err(|e| format!("SetProcessWorkingSetSize 失败: {}", e))?;
    }
    log::info!("已清理自身工作集");
    Ok(())
}

/// 查询清理统计（对照 memreduct StatisticLastReduct + jietu 增强的累计两项）
#[tauri::command]
pub fn get_clean_stats() -> CleanStats {
    Config::load().memory_clean.stats
}

// ==================== 托盘百分比徽章（对照 main.c:L566-L658 _app_iconcreate） ====================

/// 徽章三档色（对照 main.h:L33-L39；正常档用 jietu 主题丁香紫 #C8A2C8 替代 memreduct 绿 #008040）
const BADGE_COLOR_NORMAL: u32 = 0x00C8A2C8; // COLORREF 布局 0x00BBGGRR（#C8A2C8）
const BADGE_COLOR_WARNING: u32 = 0x004080FF; // #FF8040 橙
const BADGE_COLOR_DANGER: u32 = 0x00241CEC; // #EC1C24 红

/// 上次徽章百分比缓存（防闪烁，对照 main.c:L706-L712 config.ms_prev）
static LAST_BADGE_PERCENT: AtomicU32 = AtomicU32::new(u32::MAX);

/// GDI 绘制 32×32 百分比徽章 → RGBA 图像
///
/// 流程对照 _app_iconcreate（main.c:L617-L650）：DIB 位图 → 填充底色块 →
/// 居中白色数字（DT_CENTER|DT_VCENTER|DT_SINGLELINE）→ 读像素转 RGBA。
/// 徽章为满色块（无透明区域），alpha 全 255。
///
/// `warning`/`critical`：三档色分界（对照 main.c:L596-L610 _app_getdangervalue/
/// _app_getwarningvalue，默认 70/90 来自配置 danger_warning/danger_critical）。
fn render_percent_badge(
    percent: u32,
    warning: u32,
    critical: u32,
) -> Option<tauri::image::Image<'static>> {
    const SIZE: i32 = 32;
    unsafe {
        // 1. 32bpp 顶向下 DIB（对照 main.c:L618 SelectObject(hdc, hbitmap) 的绘制目标）
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = SIZE;
        bmi.bmiHeader.biHeight = -SIZE; // 负值 = 顶向下
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = 0; // BI_RGB

        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(
            hdc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            HANDLE::default(), // hSection 置空（系统分配内存）
            0,
        )
        .ok()?;
        if bits.is_null() {
            let _ = DeleteDC(hdc);
            return None;
        }
        let old_bmp = SelectObject(hdc, hbmp);

        // 2. 底色块（对照 _app_drawbackground main.c:L540-L564 + 三档色判定 L596-L610）
        let rect = RECT {
            left: 0,
            top: 0,
            right: SIZE,
            bottom: SIZE,
        };
        let color = if percent >= critical {
            BADGE_COLOR_DANGER
        } else if percent >= warning {
            BADGE_COLOR_WARNING
        } else {
            BADGE_COLOR_NORMAL
        };
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(hdc, &rect, brush);
        let _ = DeleteObject(brush);

        // 3. 白色数字（对照 main.c:L624 DT_VCENTER|DT_CENTER|DT_SINGLELINE|DT_NOCLIP|DT_NOPREFIX）
        let font = CreateFontW(
            -14, // 字符高度（32px 徽章内两位/三位数字）
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0x00FFFFFF)); // 白色（对照 TRAY_COLOR_TEXT main.h:L36）

        let mut text: Vec<u16> = format!("{}", percent).encode_utf16().collect();
        let mut text_rect = rect;
        DrawTextW(
            hdc,
            &mut text,
            &mut text_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );

        // 4. 读取 DIB 像素：BGRA → RGBA，alpha 全不透明
        let px_count = (SIZE as usize) * (SIZE as usize) * 4;
        let px = std::slice::from_raw_parts(bits as *const u8, px_count);
        let mut rgba = Vec::with_capacity(px_count);
        for c in px.chunks_exact(4) {
            rgba.extend_from_slice(&[c[2], c[1], c[0], 255]); // B G R → R G B A
        }

        // 5. 释放 GDI 资源（对照 main.c:L626-L629 恢复选择 + 隐式释放）
        SelectObject(hdc, old_bmp);
        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc);

        Some(tauri::image::Image::new_owned(
            rgba,
            SIZE as u32,
            SIZE as u32,
        ))
    }
}

/// 更新托盘百分比徽章 + Tooltip（对照 main.c:L706-L724）
///
/// - 托盘句柄经 `app.tray_by_id("main-tray")` 获取（lib.rs TrayIconBuilder::with_id 创建）
/// - 仅百分比变化时重绘（防闪烁，对照 main.c:L707 ms_prev 缓存判定）
/// - Tooltip 同步显示百分比与已用/总量（对照 main.c:L714-L724 _r_tray_setinfoformat）
pub fn update_tray_badge(app: &AppHandle, info: &MemoryInfo) {
    let percent = (info.physical.percent as u32).clamp(0, 100);

    // 防闪烁：与上次百分比相同则跳过（对照 main.c:L707）
    let prev = LAST_BADGE_PERCENT.load(Ordering::Relaxed);
    if prev == percent {
        return;
    }
    LAST_BADGE_PERCENT.store(percent, Ordering::Relaxed);

    let Some(tray) = app.tray_by_id("main-tray") else {
        return; // 托盘未创建（如测试环境）
    };

    // 三档色分界取配置（默认 70/90）
    let cfg = Config::load();
    let (warning, critical) = (
        cfg.memory_clean.danger_warning,
        cfg.memory_clean.danger_critical,
    );

    if let Some(image) = render_percent_badge(percent, warning, critical) {
        if let Err(e) = tray.set_icon(Some(image)) {
            log::warn!("托盘徽章更新失败: {}", e);
        }
    }

    // Tooltip：jietu-hdr · 内存 62% · 已用 12.4 GB/16.0 GB（设计 4.2.3）
    let tooltip = format!(
        "jietu-hdr · 内存 {}% · 已用 {}/{}",
        percent,
        format_bytes(info.physical.used),
        format_bytes(info.physical.total)
    );
    if let Err(e) = tray.set_tooltip(Some(tooltip)) {
        log::warn!("托盘 Tooltip 更新失败: {}", e);
    }
}

// ==================== 1 秒调度器（对照 main.c:L660-L704 _app_timercallback） ====================

/// 调度器单次 tick 的自动清理判定（仅提权态调用）
///
/// 对照 main.c:L679-L704：
/// - 阈值触发：percent ≥ threshold 且距上次清理 ≥ cooldown（冷却防清理循环，上游 issue #191）
/// - 间隔触发：距上次清理 ≥ interval_minutes × 60
/// - 两条件任一满足 → 清理（source=auto，自动排除危险区域）
fn auto_clean_tick(app: &AppHandle, info: &MemoryInfo) {
    let cfg = Config::load();
    let mc = &cfg.memory_clean;
    let now = chrono::Local::now().timestamp().max(0) as u64;
    let elapsed = now.saturating_sub(mc.stats.last_clean_ts);

    let mut is_clean = false;

    // 阈值触发 + 冷却（对照 main.c:L682-L692）
    if mc.auto_enable && (info.physical.percent as u32) >= mc.threshold_percent {
        if elapsed >= mc.cooldown_seconds as u64 {
            is_clean = true;
        }
    }

    // 间隔触发（对照 main.c:L694-L700）
    if !is_clean && mc.interval_enable {
        if elapsed >= mc.interval_minutes as u64 * 60 {
            is_clean = true;
        }
    }

    if is_clean {
        log::info!(
            "自动清理触发: 物理内存 {}%（阈值 {}% 距上次清理 {}s）",
            info.physical.percent as u32,
            mc.threshold_percent,
            elapsed
        );
        // mask=0 → 配置默认；source=auto 时 perform_clean 内部排除危险区域
        // （对照 main.c:L315-L319 + L703 _app_memoryclean(hwnd, SOURCE_AUTO, 0)）
        let result = perform_clean(0, "auto");
        emit_clean_notification(app, &result, "auto");
    }
}

/// 1 秒调度器主循环（对照 main.c:L660-L704 + main.h:L14 TIMER=1000ms）
///
/// 每 tick：刷新内存 → 推送事件 `memory://status`（定向 main 窗口；不可见时
/// 降频 5s，payload=MemoryInfo JSON）→ 托盘徽章判定（百分比变化才重绘）→
/// 自动清理判定（仅提权态）。
///
/// 说明：设计文档表述为 tokio interval，但 tokio 非 Cargo.toml 直接依赖（禁止改
/// Cargo.toml），故用专用 OS 线程 + 1s sleep 等价实现；清理为同步 NT 调用，
/// 与 memreduct WM_TIMER 回调同线程阻塞语义一致。
pub fn run_scheduler(app: AppHandle) {
    // 提权态启用清理所需权限（对照 main.c:L1700-L1707 _app_initialize）
    if is_elevated() {
        if let Err(e) = enable_cleanup_privileges() {
            log::warn!("启用清理权限失败: {}", e);
        }
    }
    log::info!("内存调度器启动（1s，提权态: {}）", is_elevated());

    // main 窗口不可见期间的连续 tick 计数（满 5 才补发一帧；可见即清零恢复 1s）
    let mut hidden_ticks: u32 = 0;

    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));

        // 1. 刷新内存信息（对照 main.c:L677 _app_getmemoryinfo）
        let info = get_memory_info();

        // 2. 事件推送（前端 memory://status 每秒刷新，设计 4.2.1）
        //    GPU 空转治理：原 app.emit 全局广播每秒唤醒全部 webview（viewer/pin/
        //    upscale 等均无消费方），实际监听者（MainView 内存 chip、SettingsView
        //    内存页）都在 main 窗口（App.vue 视图切换，无独立 settings 窗口）→
        //    emit_to 定向；main 不可见（如隐藏到托盘）时降频 5s 一发。
        //    emit_to 失败（窗口已销毁等）容忍：无人监听即无副作用，静默跳过。
        let main_visible = app
            .get_webview_window("main")
            .map(|w| w.is_visible().unwrap_or(true))
            .unwrap_or(true);
        if main_visible {
            hidden_ticks = 0;
        } else {
            hidden_ticks += 1;
        }
        if main_visible || hidden_ticks >= 5 {
            hidden_ticks = 0;
            let _ = app.emit_to("main", "memory://status", &info);
        }

        // 3. 托盘徽章判定（对照 main.c:L706-L712，内部防闪烁）
        update_tray_badge(&app, &info);

        // 4. 自动清理判定（仅提权态，对照 main.c:L680 _r_sys_iselevated 前置）
        if is_elevated() {
            auto_clean_tick(&app, &info);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NT 查询结构布局冒烟验证（设计 4.1 风险表：结构对不齐会导致清理失败）
    ///
    /// 校验 GlobalMemoryStatusEx 物理内存字段与 NT 查询（SystemPerformanceInformation /
    /// SystemMemoryListInformation）返回值处于合理量级，防止偏移错位读出垃圾值。
    #[test]
    fn memory_info_layout_sane() {
        let info = get_memory_info();

        // GlobalMemoryStatusEx 基础字段
        assert!(
            info.physical.total > 1_000_000_000,
            "物理内存总量异常: {}",
            info.physical.total
        );
        assert!(
            (0.0..=100.0).contains(&info.physical.percent),
            "物理内存百分比越界: {}",
            info.physical.percent
        );
        assert!(info.physical.used <= info.physical.total);
        assert!(info.commit.total >= info.commit.used);
        assert!(info.virtual_mem.total >= info.virtual_mem.used);

        // 页大小（x64 恒 4096）
        assert_eq!(page_size(), 4096);

        // NT 查询量级校验：缓存/备用/已修改/池均应 < 物理总量（结构偏移错位时会出现天文数字）
        assert!(
            info.system_cache < info.physical.total,
            "系统缓存异常: {}",
            info.system_cache
        );
        assert!(
            info.standby < info.physical.total,
            "备用列表异常: {}",
            info.standby
        );
        assert!(
            info.modified < info.physical.total,
            "已修改页异常: {}",
            info.modified
        );
        assert!(
            info.paged_pool < info.physical.total,
            "页面池异常: {}",
            info.paged_pool
        );
        assert!(
            info.non_paged_pool < info.physical.total,
            "非页面池异常: {}",
            info.non_paged_pool
        );

        println!(
            "[memory_info] 物理 {} / {} ({:.0}%) | 缓存 {} | 备用 {} | 已修改 {} | 页面池 {} | 非页面池 {} | 提权 {}",
            format_bytes(info.physical.used),
            format_bytes(info.physical.total),
            info.physical.percent,
            format_bytes(info.system_cache),
            format_bytes(info.standby),
            format_bytes(info.modified),
            format_bytes(info.paged_pool),
            format_bytes(info.non_paged_pool),
            is_elevated()
        );
    }

    /// 实际执行一次默认区域清理（默认 mask 均为安全区域，对照 memreduct 默认组合）
    ///
    /// `#[ignore]`：会真实修改系统内存状态，需显式 `cargo test -- --ignored` 触发，
    /// 用于与 memreduct-3.4 实测对照释放量（设计 M1 验证项）。
    /// 提权态先启用 SeProfileSingleProcessPrivilege/SeIncreaseQuotaPrivilege
    /// （对照 main.c:L1700-L1707，调度器启动时同样处理）。
    #[test]
    #[ignore]
    fn perform_clean_default_mask_smoke() {
        if is_elevated() {
            let _ = enable_cleanup_privileges();
        }
        let result = perform_clean(0, "cmdline");
        for r in &result.per_region {
            println!(
                "[clean] 0x{:02X} {:?} => ok={} err={:?}",
                r.mask, r.name, r.ok, r.error
            );
        }
        println!("[clean] 释放 {}", format_bytes(result.freed_bytes));
        // 默认 mask 应覆盖全部 6 个安全区域
        assert_eq!(result.per_region.len(), 6, "默认区域数应为 6");
        assert!(result.per_region.iter().any(|r| r.mask == WORKINGSET));
        // 提权态下特权区域应全部成功（普通权限下 NT 调用返回 PRIVILEGE_NOT_HELD 属预期）
        if is_elevated() {
            for r in &result.per_region {
                assert!(r.ok, "提权态区域失败: {} err={:?}", r.name, r.error);
            }
        }
    }
}
