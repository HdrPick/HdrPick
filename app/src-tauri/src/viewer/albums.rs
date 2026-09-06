//! 逻辑相册：纯虚拟文件夹树 + 图片路径引用（不对应磁盘任何目录）
//!
//! 设计：
//! - 数据文件：%APPDATA%\jietu-hdr\albums\albums.json（当前生效的组织方式）；
//!   导入/导出即整份 JSON 复制（replace 覆盖 / merge 顶层追加并重生成 id）
//! - 条目只存磁盘绝对路径 + 收录时快照（宽高/格式/mtime，只读文件头），
//!   缩略图由前端经 get_thumbnail 懒加载，双击打开时才真正读取磁盘原文件
//! - 文件夹树任意嵌套；id 用「纳秒时间戳-递增计数」生成，保证进程内唯一
//! - 写入原子化（tmp + rename），防中途崩溃损坏 JSON

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::viewer::decode;

// ==================== 对外数据结构（字段名 snake_case，与前端 interface 对齐） ====================

/// 单张收录图片（仅存磁盘路径 + 收录时快照，不复制文件）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumItem {
    /// 磁盘绝对路径（打开时才读取）
    pub path: String,
    /// 文件名（含扩展名）
    pub name: String,
    /// 宽（收录时快照；解析失败为 0）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 规范化格式名（小写扩展名，tif 统一为 tiff）
    pub format: String,
    /// 源文件修改时间（Unix 秒，收录时快照）
    pub mtime: u64,
    /// 收录时间（Unix 秒）
    pub added: u64,
}

/// 逻辑相册文件夹（可任意嵌套，不对应磁盘目录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumFolder {
    /// 唯一 id
    pub id: String,
    /// 显示名
    pub name: String,
    /// 子相册
    pub children: Vec<AlbumFolder>,
    /// 直接收录的图片（子相册内的不重复计入）
    pub items: Vec<AlbumItem>,
}

/// albums.json 完整结构（albums_state / 各变更命令返回值）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumFile {
    /// 结构版本（当前 1）
    pub version: u32,
    /// 顶层相册列表
    pub folders: Vec<AlbumFolder>,
}

// ==================== 命令 ====================

/// 读取逻辑相册当前状态
#[tauri::command]
pub fn albums_state() -> Result<AlbumFile, String> {
    load()
}

/// 新建相册文件夹（parent_id=None 时建在顶层）
#[tauri::command]
pub fn album_create_folder(parent_id: Option<String>, name: String) -> Result<AlbumFile, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("相册名称不能为空".to_string());
    }
    let mut f = load()?;
    let fid = new_id();
    let folder = AlbumFolder {
        id: fid.clone(),
        name,
        children: Vec::new(),
        items: Vec::new(),
    };
    match parent_id {
        Some(pid) => {
            let parent = find_mut(&mut f.folders, &pid).ok_or("父相册不存在")?;
            parent.children.push(folder);
        }
        None => f.folders.push(folder),
    }
    save(&f)?;
    log::info!("[逻辑相册] 新建文件夹: {}", folder_name_or(&f, &fid));
    Ok(f)
}

/// 重命名相册文件夹
#[tauri::command]
pub fn album_rename_folder(id: String, name: String) -> Result<AlbumFile, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("相册名称不能为空".to_string());
    }
    let mut f = load()?;
    let folder = find_mut(&mut f.folders, &id).ok_or("相册不存在")?;
    folder.name = name.clone();
    save(&f)?;
    log::info!("[逻辑相册] 重命名文件夹 → {}", name);
    Ok(f)
}

/// 删除相册文件夹（连同子相册；仅删除逻辑组织，磁盘文件不动）
#[tauri::command]
pub fn album_delete_folder(id: String) -> Result<AlbumFile, String> {
    let mut f = load()?;
    if !remove_folder(&mut f.folders, &id) {
        return Err("相册不存在".to_string());
    }
    save(&f)?;
    log::info!("[逻辑相册] 删除文件夹: {}", id);
    Ok(f)
}

/// 收录图片进相册（快照宽高/格式/mtime；同相册内按路径去重）
#[tauri::command]
pub fn album_add_items(folder_id: String, paths: Vec<String>) -> Result<AlbumFile, String> {
    let mut f = load()?;
    let folder = find_mut(&mut f.folders, &folder_id).ok_or("相册不存在")?;
    let added = now_secs();
    for path in paths {
        // 支持扩展名过滤（与相册库一致）；不支持的静默跳过
        let ext = Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if !is_supported_ext(&ext) {
            continue;
        }
        // 同相册内去重（Windows 路径大小写不敏感）
        if folder
            .items
            .iter()
            .any(|it| it.path.eq_ignore_ascii_case(&path))
        {
            continue;
        }
        let (w, h) = decode::quick_dimensions(Path::new(&path));
        folder.items.push(AlbumItem {
            name: Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            mtime: fs_mtime(Path::new(&path)),
            path,
            width: w,
            height: h,
            format: if ext == "tif" {
                "tiff".to_string()
            } else {
                ext
            },
            added,
        });
    }
    save(&f)?;
    log::info!("[逻辑相册] 收录图片完成: 文件夹 {}", folder_id);
    Ok(f)
}

/// 从相册移出图片（仅移出引用，磁盘文件不动）
#[tauri::command]
pub fn album_remove_items(folder_id: String, paths: Vec<String>) -> Result<AlbumFile, String> {
    let mut f = load()?;
    let folder = find_mut(&mut f.folders, &folder_id).ok_or("相册不存在")?;
    folder
        .items
        .retain(|it| !paths.iter().any(|p| p.eq_ignore_ascii_case(&it.path)));
    save(&f)?;
    log::info!("[逻辑相册] 移出 {} 张: 文件夹 {}", paths.len(), folder_id);
    Ok(f)
}

/// 跨相册移动图片（保留条目快照，目标相册内按路径去重）
#[tauri::command]
pub fn album_move_items(
    from_folder: String,
    to_folder: String,
    paths: Vec<String>,
) -> Result<AlbumFile, String> {
    if from_folder == to_folder {
        return load();
    }
    let mut f = load()?;
    let src = find_mut(&mut f.folders, &from_folder).ok_or("源相册不存在")?;
    let mut moved: Vec<AlbumItem> = Vec::new();
    src.items.retain(|it| {
        if paths.iter().any(|p| p.eq_ignore_ascii_case(&it.path)) {
            moved.push(it.clone());
            false
        } else {
            true
        }
    });
    let dst = find_mut(&mut f.folders, &to_folder).ok_or("目标相册不存在")?;
    for it in moved {
        if !dst
            .items
            .iter()
            .any(|x| x.path.eq_ignore_ascii_case(&it.path))
        {
            dst.items.push(it);
        }
    }
    save(&f)?;
    log::info!("[逻辑相册] 移动图片: {} → {}", from_folder, to_folder);
    Ok(f)
}

/// 移动相册文件夹到新父级（parent_id=None 移到顶层；禁止移入自身或其子孙）
#[tauri::command]
pub fn album_move_folder(id: String, parent_id: Option<String>) -> Result<AlbumFile, String> {
    let mut f = load()?;
    let Some(node) = take_folder(&mut f.folders, &id) else {
        return Err("相册不存在".to_string());
    };
    let dest = parent_id.clone();
    match parent_id {
        Some(pid) => {
            if pid == id || contains_id(&node, &pid) {
                return Err("不能移动到自身或其子相册内".to_string());
            }
            let parent = find_mut(&mut f.folders, &pid).ok_or("目标父相册不存在")?;
            parent.children.push(node);
        }
        None => f.folders.push(node),
    }
    save(&f)?;
    log::info!("[逻辑相册] 移动文件夹: {} → {:?}", id, dest);
    Ok(f)
}

/// 导出当前相册配置到指定 JSON 文件
#[tauri::command]
pub fn albums_export(path: String) -> Result<(), String> {
    let f = load()?;
    let s = serde_json::to_string_pretty(&f).map_err(|e| format!("序列化失败: {:#}", e))?;
    std::fs::write(&path, s).map_err(|e| format!("写入失败: {:#}", e))?;
    log::info!("[逻辑相册] 已导出配置: {}", path);
    Ok(())
}

/// 从 JSON 文件导入相册配置（mode: replace=替换组织方式 / merge=顶层追加）
#[tauri::command]
pub fn albums_import(path: String, mode: String) -> Result<AlbumFile, String> {
    let s = std::fs::read_to_string(&path).map_err(|e| format!("读取失败: {:#}", e))?;
    let imported: AlbumFile =
        serde_json::from_str(&s).map_err(|e| format!("JSON 解析失败: {:#}", e))?;
    if imported.version != 1 {
        return Err(format!("不兼容的版本: {}", imported.version));
    }
    let mut cur = load()?;
    if mode == "merge" {
        // 合并：重生成全部 id 防与现有冲突，顶层追加
        let mut folders = imported.folders;
        reassign_ids(&mut folders);
        cur.folders.extend(folders);
    } else {
        cur = imported;
    }
    save(&cur)?;
    log::info!("[逻辑相册] 已导入配置({}): {}", mode, path);
    Ok(cur)
}

// ==================== 持久化 ====================

/// 数据目录（%APPDATA%\jietu-hdr\albums）
fn albums_dir() -> Result<PathBuf, String> {
    let base = dirs::config_dir()
        .map(|d| d.join("jietu-hdr").join("albums"))
        .ok_or_else(|| "无法定位配置目录".to_string())?;
    std::fs::create_dir_all(&base).map_err(|e| format!("{:#}", e))?;
    Ok(base)
}

/// 读取相册配置（不存在返回空结构）
fn load() -> Result<AlbumFile, String> {
    let p = albums_dir()?.join("albums.json");
    if !p.exists() {
        return Ok(AlbumFile {
            version: 1,
            folders: Vec::new(),
        });
    }
    let s = std::fs::read_to_string(&p).map_err(|e| format!("{:#}", e))?;
    serde_json::from_str(&s).map_err(|e| format!("albums.json 解析失败: {:#}", e))
}

/// 原子写入（tmp + rename）
fn save(f: &AlbumFile) -> Result<(), String> {
    let dir = albums_dir()?;
    let s = serde_json::to_string_pretty(f).map_err(|e| format!("序列化失败: {:#}", e))?;
    let tmp = dir.join("albums.json.tmp");
    std::fs::write(&tmp, &s).map_err(|e| format!("写入失败: {:#}", e))?;
    std::fs::rename(&tmp, dir.join("albums.json")).map_err(|e| format!("落盘失败: {:#}", e))
}

// ==================== 工具 ====================

/// 生成文件夹 id：纳秒时间戳 + 进程内递增计数（唯一性足够）
fn new_id() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    format!("f{:x}-{:x}", n, c)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fs_mtime(p: &Path) -> u64 {
    std::fs::metadata(p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 递归查找（可变引用）
fn find_mut<'a>(folders: &'a mut Vec<AlbumFolder>, id: &str) -> Option<&'a mut AlbumFolder> {
    for f in folders.iter_mut() {
        if f.id == id {
            return Some(f);
        }
        if let Some(hit) = find_mut(&mut f.children, id) {
            return Some(hit);
        }
    }
    None
}

/// 从树中摘出整个文件夹子树
fn take_folder(folders: &mut Vec<AlbumFolder>, id: &str) -> Option<AlbumFolder> {
    if let Some(pos) = folders.iter().position(|f| f.id == id) {
        return Some(folders.remove(pos));
    }
    for f in folders.iter_mut() {
        if let Some(hit) = take_folder(&mut f.children, id) {
            return Some(hit);
        }
    }
    None
}

/// 删除文件夹（递归；返回是否删到）
fn remove_folder(folders: &mut Vec<AlbumFolder>, id: &str) -> bool {
    let before = folders.len();
    folders.retain(|f| f.id != id);
    if folders.len() != before {
        return true;
    }
    folders
        .iter_mut()
        .any(|f| remove_folder(&mut f.children, id))
}

/// 子树内是否包含指定 id（移动文件夹防环）
fn contains_id(f: &AlbumFolder, id: &str) -> bool {
    f.id == id || f.children.iter().any(|c| contains_id(c, id))
}

/// merge 导入时递归重生成全部 id
fn reassign_ids(folders: &mut Vec<AlbumFolder>) {
    for f in folders.iter_mut() {
        f.id = new_id();
        reassign_ids(&mut f.children);
    }
}

/// 日志用：按 id 找显示名（找不到就回退 id）
fn folder_name_or(f: &AlbumFile, id: &str) -> String {
    fn walk(folders: &[AlbumFolder], id: &str) -> Option<String> {
        for f in folders {
            if f.id == id {
                return Some(f.name.clone());
            }
            if let Some(hit) = walk(&f.children, id) {
                return Some(hit);
            }
        }
        None
    }
    walk(&f.folders, id).unwrap_or_else(|| id.to_string())
}

/// 支持的扩展名清单（与相册库一致）
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
