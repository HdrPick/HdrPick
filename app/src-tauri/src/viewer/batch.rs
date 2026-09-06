//! 批量重命名：预览 + 执行
//!
//! 设计：
//! - 预览：纯字符串计算（不落盘）。新名 = prefix + 日期（可选）+ 序号 + 原扩展名，
//!   序号从 startNum 起 zero-pad 到 digits 位；日期取当前本地时间按 dateFmt 格式化
//! - 执行：逐对在源文件同目录 rename；目标已存在 / 源不存在 / 新名非法 计入失败列表
//!   返回（不中断其余条目）；仅大小写变化的重命名放行（Windows 大小写不敏感会误判
//!   “目标已存在”）
//! - 日期格式化手写令牌替换（yyyy/MM/dd/HH/mm/ss，其余字符原样），本地时区偏移取自
//!   Windows GetTimeZoneInformation（与 set_wallpaper 同款 extern "system" 直调约定），
//!   不引入 chrono
//! - 执行属重活：tokio::task::spawn_blocking 包裹，避免阻塞 async 运行时

use std::path::{Path, PathBuf};

use serde::Deserialize;

// ==================== 对外数据结构 ====================

/// 重命名对（前端 pairs: [{ old, new }]）
#[derive(Debug, Clone, Deserialize)]
pub struct RenamePair {
    /// 源文件完整路径
    pub old: String,
    /// 新文件名（不含目录）
    pub new: String,
}

// ==================== 命令 ====================

/// 预览批量重命名：对每个文件名生成新文件名（返回顺序与输入一致，不落盘）
#[tauri::command]
pub fn batch_rename_preview(
    names: Vec<String>,
    prefix: String,
    startNum: u32,
    digits: u32,
    useDate: bool,
    dateFmt: String,
) -> Vec<String> {
    // 日期只格式化一次，逐项拼接
    let date_part = if useDate {
        format_date_now(&dateFmt)
    } else {
        String::new()
    };
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let num = format!(
                "{:0width$}",
                startNum.saturating_add(i as u32),
                width = digits as usize
            );
            format!("{}{}{}{}", prefix, date_part, num, ext_with_dot(name))
        })
        .collect()
}

/// 执行批量重命名：返回失败列表（形如 “old → new：原因”），成功项不返回
#[tauri::command]
pub async fn batch_rename(pairs: Vec<RenamePair>) -> Vec<String> {
    let total = pairs.len();
    let failures = tokio::task::spawn_blocking(move || {
        pairs
            .iter()
            .filter_map(|p| rename_one(&p.old, &p.new).err())
            .collect::<Vec<String>>()
    })
    .await
    .unwrap_or_else(|e| vec![format!("重命名任务执行失败: {}", e)]);

    log::info!(
        "批量重命名完成：共 {} 项，失败 {} 项",
        total,
        failures.len()
    );
    failures
}

// ==================== 内部辅助 ====================

/// 取文件扩展名（带点；无扩展名为空串）——保留原扩展名拼接用
fn ext_with_dot(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default()
}

/// 单条重命名：源须存在且为文件；新名须为合法文件名（不含路径分隔符）；
/// 目标已存在则跳过并返回错误说明；仅大小写变化时放行
fn rename_one(old: &str, new: &str) -> Result<(), String> {
    let old_path = PathBuf::from(old);
    if !old_path.is_file() {
        return Err(format!("{}：源文件不存在", old));
    }
    let new_name = new.trim();
    if new_name.is_empty() {
        return Err(format!("{}：新文件名为空", old));
    }
    if new_name.contains('/') || new_name.contains('\\') || new_name == "." || new_name == ".." {
        return Err(format!(
            "{} → {}：新文件名非法（不能包含路径分隔符）",
            old, new_name
        ));
    }
    let Some(parent) = old_path.parent().map(|p| p.to_path_buf()) else {
        return Err(format!("{}：路径缺少父目录", old));
    };
    let new_path = parent.join(new_name);
    if old_path == new_path {
        return Ok(()); // 原名原样：无操作视为成功
    }
    // 仅大小写变化（同目录、同名忽略大小写）：Windows 会把 new_path 误判为已存在，直接放行
    let case_only = old_path
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case(new_name))
        .unwrap_or(false);
    if !case_only && new_path.exists() {
        return Err(format!("{} → {}：目标已存在，已跳过", old, new_name));
    }
    std::fs::rename(&old_path, &new_path).map_err(|e| format!("{} → {}：{:#}", old, new_name, e))
}

// ==================== 手写日期格式化（不引 chrono） ====================

/// 本地当前时间分量
struct CivilNow {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

/// 当前本地时间按 fmt 格式化
fn format_date_now(fmt: &str) -> String {
    format_date(fmt, &local_now())
}

/// 简易令牌替换：yyyy / MM / dd / HH / mm / ss，其余字符原样输出
/// （按最长令牌优先在当前位置匹配，UTF-8 边界安全）
fn format_date(fmt: &str, t: &CivilNow) -> String {
    let tokens: [(&str, String); 6] = [
        ("yyyy", format!("{:04}", t.year)),
        ("MM", format!("{:02}", t.month)),
        ("dd", format!("{:02}", t.day)),
        ("HH", format!("{:02}", t.hour)),
        ("mm", format!("{:02}", t.minute)),
        ("ss", format!("{:02}", t.second)),
    ];
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut rest = fmt;
    'outer: while !rest.is_empty() {
        for (tok, val) in &tokens {
            if rest.starts_with(tok) {
                out.push_str(val);
                rest = &rest[tok.len()..];
                continue 'outer;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// 当前本地时间（Unix 秒 + 本地偏移 → 公历日期）
fn local_now() -> CivilNow {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let local = secs + local_offset_secs();
    let days = local.div_euclid(86_400);
    let sod = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    CivilNow {
        year,
        month,
        day,
        hour: (sod / 3600) as u32,
        minute: ((sod % 3600) / 60) as u32,
        second: (sod % 60) as u32,
    }
}

/// 本地时区偏移（秒，东八区为 +28800）——Windows 直调 GetTimeZoneInformation
#[cfg(windows)]
fn local_offset_secs() -> i64 {
    #[repr(C)]
    #[allow(dead_code)]
    struct SystemTimeW {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[repr(C)]
    #[allow(dead_code)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTimeW,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTimeW,
        daylight_bias: i32,
    }
    extern "system" {
        fn GetTimeZoneInformation(tzi: *mut TimeZoneInformation) -> u32;
    }
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    unsafe {
        let mut tzi: TimeZoneInformation = std::mem::zeroed();
        let ret = GetTimeZoneInformation(&mut tzi);
        // Windows 约定：UTC = 本地时间 + Bias（分钟）；夏令时生效时再加 DaylightBias
        let mut bias = tzi.bias;
        if ret == TIME_ZONE_ID_DAYLIGHT {
            bias += tzi.daylight_bias;
        }
        -(bias as i64) * 60
    }
}

/// 非 Windows 兜底：按东八区
#[cfg(not(windows))]
fn local_offset_secs() -> i64 {
    8 * 3600
}

/// 天数（自 1970-01-01）→ 公历 (年, 月, 日)（Howard Hinnant civil_from_days 算法）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}
