//! 软件回收站（设计稿 3.3 回收站机制：删除图片移入软件回收目录，7 天可恢复）
//!
//! 设计：
//! - 回收目录：%APPDATA%\jietu-hdr\recycle\
//! - 元数据：recycle\index.json（数组，每项记录原始路径/回收文件名/时间戳）
//! - 删除 = 移动文件到回收目录（重命名为 <时间戳>_<原名> 防冲突）
//! - 恢复 = 移回原始路径（原目录不存在时自动创建；目标被占用则追加序号）
//! - 彻底删除 = 删除回收文件；清空 = 全部彻底删除
//! - 7 天自动过期：每次启动/查询时惰性清理

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 单条回收记录
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecycleEntry {
    /// 原始完整路径（恢复目标）
    pub original_path: String,
    /// 回收目录内的文件名
    pub recycled_name: String,
    /// 回收 Unix 时间戳（秒）
    pub deleted_ts: u64,
    /// 原文件名（展示用）
    pub display_name: String,
}

/// 回收目录（%APPDATA%\jietu-hdr\recycle）
fn recycle_dir() -> Result<PathBuf, String> {
    let base = dirs::config_dir()
        .map(|d| d.join("jietu-hdr").join("recycle"))
        .ok_or_else(|| "无法定位配置目录".to_string())?;
    std::fs::create_dir_all(&base).map_err(|e| format!("{:#}", e))?;
    Ok(base)
}

/// 索引文件路径
fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

/// 读取索引（不存在返回空；顺带惰性清理 7 天过期项）
fn load_index() -> Result<Vec<RecycleEntry>, String> {
    let dir = recycle_dir()?;
    let p = index_path(&dir);
    let list: Vec<RecycleEntry> = if p.exists() {
        let s = std::fs::read_to_string(&p).map_err(|e| format!("{:#}", e))?;
        serde_json::from_str(&s).unwrap_or_default()
    } else {
        Vec::new()
    };

    // 惰性清理：删除 7 天前的记录与文件
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let keep: Vec<RecycleEntry> = list
        .into_iter()
        .filter(|e| {
            if now.saturating_sub(e.deleted_ts) > 7 * 24 * 3600 {
                let _ = std::fs::remove_file(dir.join(&e.recycled_name));
                false
            } else {
                true
            }
        })
        .collect();
    if keep.len() != 0 || p.exists() {
        save_index(&dir, &keep)?;
    }
    Ok(keep)
}

/// 写入索引
fn save_index(dir: &Path, list: &[RecycleEntry]) -> Result<(), String> {
    let s = serde_json::to_string_pretty(list).map_err(|e| format!("{:#}", e))?;
    std::fs::write(index_path(dir), s).map_err(|e| format!("{:#}", e))?;
    Ok(())
}

/// 移入回收站
///
/// 移动文件到回收目录；索引记录原始路径与时间戳。
pub fn recycle_file(path: &str) -> Result<RecycleEntry, String> {
    let src = Path::new(path);
    if !src.exists() {
        return Err(format!("文件不存在: {}", path));
    }
    let dir = recycle_dir()?;
    let mut list = load_index()?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let display_name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "未命名".into());
    // 回收名：<时间戳>_<原名>（防同目录重名冲突）
    let recycled_name = format!("{}_{}", now, display_name);

    std::fs::rename(src, dir.join(&recycled_name))
        .map_err(|e| format!("移动到回收站失败: {:#}", e))?;

    let entry = RecycleEntry {
        original_path: path.to_string(),
        recycled_name,
        deleted_ts: now,
        display_name,
    };
    list.push(entry.clone());
    save_index(&dir, &list)?;
    log::info!("已移入回收站: {}", path);
    Ok(entry)
}

/// 恢复到原始路径（原目录不存在则创建；目标已存在同名文件时追加序号）
pub fn restore_file(recycled_name: &str) -> Result<String, String> {
    let dir = recycle_dir()?;
    let mut list = load_index()?;
    let idx = list
        .iter()
        .position(|e| e.recycled_name == recycled_name)
        .ok_or_else(|| "回收站中未找到该记录".to_string())?;
    let entry = list[idx].clone();

    let src = dir.join(&entry.recycled_name);
    if !src.exists() {
        // 文件已丢失：仅清记录
        list.remove(idx);
        save_index(&dir, &list)?;
        return Err("回收文件已丢失，记录已清理".into());
    }

    let mut dst = PathBuf::from(&entry.original_path);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{:#}", e))?;
    }
    // 目标被占用 → 追加 (1)/(2)…
    if dst.exists() {
        let stem = dst
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = dst
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        let parent = dst.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        for i in 1..1000 {
            let cand = parent.join(format!("{}({}){}", stem, i, ext));
            if !cand.exists() {
                dst = cand;
                break;
            }
        }
    }
    std::fs::rename(&src, &dst).map_err(|e| format!("恢复失败: {:#}", e))?;

    list.remove(idx);
    save_index(&dir, &list)?;
    let restored = dst.to_string_lossy().into_owned();
    log::info!("已从回收站恢复: {} → {}", entry.original_path, restored);
    Ok(restored)
}

/// 彻底删除单条（删除回收文件 + 记录）
pub fn purge_file(recycled_name: &str) -> Result<(), String> {
    let dir = recycle_dir()?;
    let mut list = load_index()?;
    let idx = list
        .iter()
        .position(|e| e.recycled_name == recycled_name)
        .ok_or_else(|| "回收站中未找到该记录".to_string())?;
    let _ = std::fs::remove_file(dir.join(&list[idx].recycled_name));
    list.remove(idx);
    save_index(&dir, &list)?;
    log::info!("已彻底删除回收项: {}", recycled_name);
    Ok(())
}

/// 清空回收站
pub fn purge_all() -> Result<usize, String> {
    let dir = recycle_dir()?;
    let list = load_index()?;
    let n = list.len();
    for e in &list {
        let _ = std::fs::remove_file(dir.join(&e.recycled_name));
    }
    save_index(&dir, &[])?;
    log::info!("已清空回收站（{} 项）", n);
    Ok(n)
}

/// 列出回收站（已过滤 7 天过期）
pub fn list_files() -> Result<Vec<RecycleEntry>, String> {
    load_index()
}
