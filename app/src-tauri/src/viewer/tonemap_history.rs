//! 调参历史（tonemap history）：参数 sidecar + 文件夹预解码
//!
//! **存储方案 = 存参数（不存渲染图）**：每条历史仅几十字节（时间戳 + 预设 id +
//! overrides 快照），sidecar 文件放原图同目录 `.jietu-hdr/<文件名>.json`，
//! 跟随图片移动/复制。打开图片时自动读回历史；选任意一条即可无损重渲
//! （tone map 是纯函数，同参数必得同图）。
//!
//! **预解码**：看图打开 HDR 图时后台解码同目录所有 HDR 图的 scRGB 线性源
//! （LRU 8 张，decode::HDR_SOURCE_CACHE），文件夹内切换零解码等待。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::viewer::decode::{self, ImageKind};

// ==================== Sidecar 数据结构 ====================

/// 单条调参历史（参数快照；重渲 = 用这些参数跑 retonemap）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 记录时间（Unix 毫秒）
    pub ts: u64,
    /// 预设 id（如 builtin:soft）
    pub preset: String,
    /// 参数覆盖快照（None 字段 = 跟随预设）
    pub overrides: crate::config::ToneMapOverrides,
    /// 用户备注（可选）
    #[serde(default)]
    pub note: Option<String>,
}

/// Sidecar 根结构
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryFile {
    /// 历史记录（新→旧；上限 50 条，超出丢最旧）
    pub entries: Vec<HistoryEntry>,
    /// 当前应用的条目 ts（打开时自动套用；0 = 无）
    pub applied: u64,
}

const HISTORY_MAX: usize = 50;

/// sidecar 路径：原图同目录 .jietu-hdr/<stem>.json
fn sidecar_path(img: &Path) -> PathBuf {
    let dir = img.parent().unwrap_or_else(|| Path::new("."));
    let stem = img
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string());
    dir.join(".jietu-hdr").join(format!("{}.json", stem))
}

/// 读历史（无 sidecar 返回空结构）
fn load(img: &Path) -> HistoryFile {
    std::fs::read_to_string(sidecar_path(img))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 写历史（原子替换：先 tmp 后 rename）
fn save(img: &Path, h: &HistoryFile) -> Result<(), String> {
    let p = sidecar_path(img);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("创建历史目录失败: {}", e))?;
    }
    let json = serde_json::to_string_pretty(h).map_err(|e| format!("序列化失败: {}", e))?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("写历史失败: {}", e))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("原子替换失败: {}", e))?;
    Ok(())
}

// ==================== 命令 ====================

/// 追加一条调参历史（TonePanel 每次重渲后调用；去重：同参数 60s 内不重复记）
#[tauri::command]
pub fn history_add(
    path: String,
    preset: String,
    overrides: crate::config::ToneMapOverrides,
) -> Result<HistoryEntry, String> {
    let p = PathBuf::from(&path);
    let mut h = load(&p);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // 去重：与最新一条参数完全相同且 60s 内 → 刷新 applied 即可
    if let Some(top) = h.entries.first() {
        if top.preset == preset
            && format!("{:?}", top.overrides) == format!("{:?}", overrides)
            && now.saturating_sub(top.ts) < 60_000
        {
            h.applied = top.ts;
            save(&p, &h)?;
            return Ok(top.clone());
        }
    }
    let entry = HistoryEntry {
        ts: now,
        preset,
        overrides,
        note: None,
    };
    h.entries.insert(0, entry.clone());
    h.entries.truncate(HISTORY_MAX);
    h.applied = now;
    save(&p, &h)?;
    log::info!("调参历史：新增 1 条（{}，共 {} 条）", path, h.entries.len());
    Ok(entry)
}

/// 读历史（含当前应用条目；打开图片时前端调用）
#[tauri::command]
pub fn history_list(path: String) -> Result<HistoryFile, String> {
    Ok(load(&PathBuf::from(&path)))
}

/// 标记当前应用条目（选中某条重渲后调用；对比模式选图不改 applied）
#[tauri::command]
pub fn history_apply(path: String, ts: u64) -> Result<(), String> {
    let p = PathBuf::from(&path);
    let mut h = load(&p);
    if h.entries.iter().any(|e| e.ts == ts) {
        h.applied = ts;
        save(&p, &h)?;
    }
    Ok(())
}

/// 删除一条历史
#[tauri::command]
pub fn history_delete(path: String, ts: u64) -> Result<(), String> {
    let p = PathBuf::from(&path);
    let mut h = load(&p);
    h.entries.retain(|e| e.ts != ts);
    if h.applied == ts {
        h.applied = h.entries.first().map(|e| e.ts).unwrap_or(0);
    }
    save(&p, &h)?;
    Ok(())
}

/// 重生图：按某条历史的参数把 HDR 源渲成 SDR PNG，输出到原图同目录
///
/// 文件名 = `<原名>_<预设名>_<MMdd-HHmmss>.png`（预设名去掉 builtin: 前缀）。
/// 返回完整输出路径（前端 toast 展示）。
#[tauri::command]
pub async fn history_export_png(path: String, ts: u64) -> Result<String, String> {
    let p = PathBuf::from(&path);
    let h = load(&p);
    let entry = h
        .entries
        .iter()
        .find(|e| e.ts == ts)
        .ok_or_else(|| "历史条目不存在".to_string())?;

    // 参数 = 该条历史的预设 + overrides
    let params = {
        let mut config = crate::config::Config::load();
        config.tonemap_settings.active_preset = entry.preset.clone();
        config.tonemap_settings.overrides = entry.overrides.clone();
        config.active_tonemap_params(crate::capture::monitor::get_sdr_white_level_nits())
    };

    // HDR 源就绪（缓存优先；未命中完整解码）→ SDR 临时 PNG
    decode::ensure_hdr_source(&p);
    let (temp, _w, _h) = decode::retonemap_to_temp_png(&p, &params)
        .ok_or_else(|| "HDR 源解码失败".to_string())?
        .map_err(|e| format!("重渲失败: {}", e))?;

    // 输出文件名：<原名>_<预设>_<时间>.png（原图同目录）
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let preset_name = entry.preset.replace("builtin:", "");
    let now = chrono::Local::now();
    let stamp = now.format("%m%d-%H%M%S");
    let out = p
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{}_{}_{}.png", stem, preset_name, stamp));

    // 临时 PNG → 目标（拷贝；临时文件留给缓存管理）
    std::fs::copy(&temp, &out).map_err(|e| format!("写入失败: {}", e))?;
    log::info!(
        "历史重生图: {} → {}（预设 {}）",
        path,
        out.display(),
        entry.preset
    );
    Ok(out.to_string_lossy().into_owned())
}

// ==================== 文件夹预解码 ====================

/// 后台预解码当前目录所有 HDR 图（scRGB 源入 LRU 缓存）
///
/// 打开 HDR 图时前端调用：当前图优先（已在缓存），其余按文件名序排队解码；
/// 后台线程执行不阻塞 UI；重复调用（连续切图）自动去重（在缓存里就跳过）。
#[tauri::command]
pub async fn prewarm_folder(dir: String) -> Result<usize, String> {
    let d = PathBuf::from(&dir);
    if !d.is_dir() {
        return Ok(0);
    }
    // 收集目录内 HDR 文件（扩展名粗筛 + classify 精筛）
    let mut hdrs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&d)
        .map_err(|e| format!("读目录失败: {}", e))?
        .flatten()
    {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        match decode::classify_with_content(&p) {
            ImageKind::Exr | ImageKind::PngHdr | ImageKind::Jxl | ImageKind::Jxr => {
                hdrs.push(p);
            }
            _ => {}
        }
    }
    let total = hdrs.len();
    // 后台队列解码（spawn_blocking 线程池；串行防内存峰值超 LRU 上限）
    tauri::async_runtime::spawn_blocking(move || {
        for (i, p) in hdrs.iter().enumerate() {
            // 已在缓存 → 跳过（预解码幂等）
            if decode::take_hdr_source(p).is_some() {
                continue;
            }
            let _ = decode::ensure_hdr_source(p);
            if i % 5 == 0 {
                log::debug!("预解码进度: {}/{}", i + 1, total);
            }
        }
        log::info!("文件夹预解码完成: {}（目录 {}）", total, dir);
    });
    Ok(total)
}
