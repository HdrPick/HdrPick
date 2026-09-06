//! 相册库：多根目录图片索引 + 元数据（星级/标签）
//!
//! 设计：
//! - 数据目录：%APPDATA%\jietu-hdr\library\
//!   - index.json：{ roots, entries }（根目录列表 + 扫描结果缓存）
//!   - meta.json：{ 路径: { stars, tags } }（用户标注，独立持久化；
//!     条目被扫描剔除后元数据保留，便于文件回归后星级/标签自动恢复）
//! - 扫描：std::fs 队列式递归遍历全部 roots；跳过隐藏/系统目录与符号链接（防环）；
//!   扩展名过滤与 viewer 主清单一致；宽高复用 decode::quick_dimensions（只读头）
//! - 增量合并：以 路径（小写）+ mtime + 文件名 为键——未变化的条目沿用旧宽高
//!   （跳过头解析），新增/变更条目重新解析，磁盘上已消失的条目剔除后持久化
//! - 扫描属重活：async 命令内用 tokio::task::spawn_blocking 包裹，避免阻塞 async 运行时

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::viewer::decode;

// ==================== 对外数据结构（字段名 snake_case，与前端 interface 对齐） ====================

/// 相册库完整状态（library_state / scan_library 返回值）
#[derive(Debug, Clone, Serialize)]
pub struct LibraryState {
    /// 已登记的根目录列表
    pub roots: Vec<String>,
    /// 当前索引条目
    pub entries: Vec<LibraryEntry>,
    /// 用户元数据（完整路径 → 星级/标签）
    pub metas: HashMap<String, MetaEntry>,
}

/// 单条图片索引（前端 interface LibraryEntry）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// 完整路径
    pub path: String,
    /// 文件名（含扩展名）
    pub name: String,
    /// 是否目录条目（当前恒为 false，预留）
    pub dir: bool,
    /// 宽（像素；解析失败为 0，前端可用 naturalWidth 兜底）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 规范化格式名（小写扩展名，tif 统一为 tiff）
    pub format: String,
    /// 修改时间（Unix 秒）
    pub mtime: u64,
}

/// 单条用户元数据（前端 interface MetaEntry）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaEntry {
    /// 星级（0-5）
    pub stars: u8,
    /// 标签列表
    pub tags: Vec<String>,
}

/// index.json 结构
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LibraryIndex {
    roots: Vec<String>,
    entries: Vec<LibraryEntry>,
}

// ==================== 命令 ====================

/// 读取相册库当前状态（根目录 + 索引缓存 + 元数据）
#[tauri::command]
pub fn library_state() -> Result<LibraryState, String> {
    let idx = load_index()?;
    let metas = load_metas()?;
    Ok(LibraryState {
        roots: idx.roots,
        entries: idx.entries,
        metas,
    })
}

/// 新增相册库根目录（校验存在；已在库中则幂等返回）
#[tauri::command]
pub fn add_library_root(path: String) -> Result<(), String> {
    if !Path::new(&path).is_dir() {
        return Err(format!("目录不存在: {}", path));
    }
    let mut idx = load_index()?;
    let added = path.trim().to_string();
    // Windows 路径大小写不敏感，去重比较忽略大小写
    if idx.roots.iter().any(|r| r.eq_ignore_ascii_case(&added)) {
        return Ok(());
    }
    idx.roots.push(added);
    save_index(&idx)?;
    log::info!("相册库新增根目录: {}", path);
    Ok(())
}

/// 移除相册库根目录（同时剔除该根目录下的索引条目；元数据保留）
#[tauri::command]
pub fn remove_library_root(path: String) -> Result<(), String> {
    let mut idx = load_index()?;
    let removed = path.trim().trim_end_matches('\\').to_lowercase();
    let Some(pos) = idx
        .roots
        .iter()
        .position(|r| r.trim().trim_end_matches('\\').to_lowercase() == removed)
    else {
        return Err(format!("该目录不在相册库根目录中: {}", path));
    };
    idx.roots.remove(pos);

    // 剔除该根目录下的全部条目（root 本身或 root\... 前缀，忽略大小写）
    let prefix = format!("{}\\", removed);
    idx.entries
        .retain(|e| !e.path.to_lowercase().starts_with(&prefix));
    save_index(&idx)?;
    log::info!("相册库移除根目录: {}", path);
    Ok(())
}

/// 递归扫描全部根目录并增量合并，持久化后返回完整状态（重活，阻塞线程池执行）
#[tauri::command]
pub async fn scan_library() -> Result<LibraryState, String> {
    let mut idx = load_index()?;
    let roots = idx.roots.clone();
    let old = idx.entries.clone();

    // 扫描 + 头解析全部放入阻塞线程池，避免卡 async 运行时
    let entries = tokio::task::spawn_blocking(move || merge_entries(old, scan_roots(&roots)))
        .await
        .map_err(|e| format!("扫描任务执行失败: {}", e))?;

    idx.entries = entries;
    save_index(&idx)?;
    let metas = load_metas()?;
    Ok(LibraryState {
        roots: idx.roots,
        entries: idx.entries,
        metas,
    })
}

/// 设置单条元数据（stars=0 且 tags 为空时删除该条，保持 meta.json 整洁）
#[tauri::command]
pub fn set_meta(path: String, stars: u8, tags: Vec<String>) -> Result<(), String> {
    let mut metas = load_metas()?;
    if stars == 0 && tags.is_empty() {
        metas.remove(&path);
    } else {
        metas.insert(path, MetaEntry { stars, tags });
    }
    save_metas(&metas)
}

// ==================== 持久化 ====================

/// 数据目录（%APPDATA%\jietu-hdr\library）
fn library_dir() -> Result<PathBuf, String> {
    let base = dirs::config_dir()
        .map(|d| d.join("jietu-hdr").join("library"))
        .ok_or_else(|| "无法定位配置目录".to_string())?;
    std::fs::create_dir_all(&base).map_err(|e| format!("{:#}", e))?;
    Ok(base)
}

/// 读取索引（不存在返回空索引）
fn load_index() -> Result<LibraryIndex, String> {
    let p = library_dir()?.join("index.json");
    if !p.exists() {
        return Ok(LibraryIndex::default());
    }
    let s = std::fs::read_to_string(&p).map_err(|e| format!("{:#}", e))?;
    serde_json::from_str(&s).map_err(|e| format!("index.json 解析失败: {:#}", e))
}

/// 写入索引
fn save_index(idx: &LibraryIndex) -> Result<(), String> {
    let s = serde_json::to_string_pretty(idx).map_err(|e| format!("{:#}", e))?;
    std::fs::write(library_dir()?.join("index.json"), s).map_err(|e| format!("{:#}", e))
}

/// 读取元数据表（不存在返回空表）
fn load_metas() -> Result<HashMap<String, MetaEntry>, String> {
    let p = library_dir()?.join("meta.json");
    if !p.exists() {
        return Ok(HashMap::new());
    }
    let s = std::fs::read_to_string(&p).map_err(|e| format!("{:#}", e))?;
    serde_json::from_str(&s).map_err(|e| format!("meta.json 解析失败: {:#}", e))
}

/// 写入元数据表
fn save_metas(metas: &HashMap<String, MetaEntry>) -> Result<(), String> {
    let s = serde_json::to_string_pretty(metas).map_err(|e| format!("{:#}", e))?;
    std::fs::write(library_dir()?.join("meta.json"), s).map_err(|e| format!("{:#}", e))
}

// ==================== 扫描与增量合并 ====================

/// 扫描轻量项（不解析宽高，是否解析交给增量合并决定）
struct ScanItem {
    path: String,
    name: String,
    /// 小写扩展名
    ext: String,
    mtime: u64,
}

/// 支持的扩展名清单（与 viewer 主清单一致，含阶段外的 jxl）
fn is_supported_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "bmp"
            | "webp"
            | "avif"
            | "tif"
            | "tiff"
            | "ico"
            | "psd"
            | "jxl"
    )
}

/// 是否隐藏/系统目录（Windows 属性位判断）
fn is_hidden_or_system(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        let attrs = meta.file_attributes();
        attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

/// 文件修改时间（Unix 秒；失败为 0）
fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 队列式递归扫描全部根目录：跳过隐藏/系统目录、符号链接，按扩展名过滤；
/// 路径去重（不同根目录嵌套时不重复收录）
fn scan_roots(roots: &[String]) -> Vec<ScanItem> {
    let mut out: Vec<ScanItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();

    while let Some(dir) = queue.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue, // 单目录失败不阻断整体扫描
        };
        for entry in rd.flatten() {
            // file_type 不跟随符号链接：链接目录/文件一律跳过（防环、防越界）
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                match entry.metadata() {
                    Ok(m) if !is_hidden_or_system(&m) => queue.push(path),
                    _ => {}
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if !is_supported_ext(&ext) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let item_path = path.to_string_lossy().into_owned();
            if !seen.insert(item_path.to_lowercase()) {
                continue;
            }
            out.push(ScanItem {
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path: item_path,
                mtime: mtime_secs(&meta),
                ext,
            });
        }
    }
    out
}

/// 增量合并：path（小写）+ mtime + name 未变化的条目沿用旧宽高（跳过头解析）；
/// 新增/变更条目重新解析；不在扫描结果中的旧条目剔除（HashMap.remove 后自然丢弃）
fn merge_entries(old: Vec<LibraryEntry>, items: Vec<ScanItem>) -> Vec<LibraryEntry> {
    let mut old_map: HashMap<String, LibraryEntry> = old
        .into_iter()
        .map(|e| (e.path.to_lowercase(), e))
        .collect();
    let mut merged = Vec::with_capacity(items.len());
    for it in items {
        let key = it.path.to_lowercase();
        let entry = match old_map.remove(&key) {
            Some(e) if e.mtime == it.mtime && e.name == it.name => e,
            _ => {
                // 新增或变更：重新读头解析宽高（单文件失败为 0 不阻断）
                let (w, h) = decode::quick_dimensions(Path::new(&it.path));
                LibraryEntry {
                    path: it.path.clone(),
                    name: it.name.clone(),
                    dir: false,
                    width: w,
                    height: h,
                    format: if it.ext == "tif" {
                        "tiff".to_string()
                    } else {
                        it.ext.clone()
                    },
                    mtime: it.mtime,
                }
            }
        };
        merged.push(entry);
    }
    merged.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    merged
}
