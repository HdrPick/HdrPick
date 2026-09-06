//! 加密隐私相册（vault）
//!
//! 设计：
//! - 存储：%APPDATA%\jietu-hdr\vault\
//!   - index.json：整体加密的索引。帧格式 [16B 盐][12B nonce][AES-256-GCM 密文+tag]，
//!     明文为 { entries: [{ id, name, width, height, added_ts, original_path }] }
//!   - data\<id>：加密图片 blob，帧格式 [12B nonce][密文+tag]；id = 32 位十六进制随机串
//! - 密钥：PBKDF2-HMAC-SHA256（120 000 迭代）由口令 + 盐派生 32 字节；
//!   主密钥仅保存在 Rust 进程内全局会话（OnceLock<Mutex<Option<...>>>），绝不下发前端；
//!   vault_lock 即清零密钥并清空 %TEMP%\jietu-hdr\vault_view\ 查看目录
//! - 加密：AES-256-GCM（aes-gcm crate），索引与每个 blob 各用独立随机 nonce；
//!   解密失败（认证不过）即视为密码错误或数据损坏
//! - 缩略图：解密到内存 → image 解码 → 200×200 保比例 → PNG base64（复用
//!   thumbnail.rs 思路），进程内 LRU 缓存 ≤ 50 项（key = 条目 id）；
//!   AVIF/PSD 无法内存解码时返回空串由前端占位
//! - 重活（PBKDF2 派生 / 加解密 / 批量重加密）全部 tokio::task::spawn_blocking，
//!   避免阻塞 async 运行时；会话锁只在快照/提交时短持有，不跨 await
//! - 改密：两阶段重加密（全部 blob 先写 \<id>.tmp → 旧数据让位 \<id>.old →
//!   新数据就位 → 删 .old + 写新索引），任一步失败可整体回退到旧口令状态

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use serde::{Deserialize, Serialize};

use crate::viewer::decode;

// ==================== 常量 ====================

/// PBKDF2 迭代次数
const PBKDF2_ITERS: u32 = 120_000;
/// 盐长度（字节）
const SALT_LEN: usize = 16;
/// GCM nonce 长度（字节）
const NONCE_LEN: usize = 12;
/// 主密钥长度（AES-256）
const KEY_LEN: usize = 32;
/// GCM 认证标签长度（字节）
const TAG_LEN: usize = 16;
/// 缩略图最长边上限
const THUMB_SIZE: u32 = 200;
/// 缩略图 LRU 缓存上限（项）
const THUMB_LRU_MAX: usize = 50;

// ==================== 对外数据结构（字段名 snake_case，与前端 interface 对齐） ====================

/// 相册状态（前端 interface VaultStatus）
#[derive(Debug, Clone, Serialize)]
pub struct VaultStatus {
    pub initialized: bool,
    pub unlocked: bool,
    pub count: usize,
}

/// 索引/列表单条（前端 interface VaultEntry）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultEntryCore {
    /// 条目 id（= data\ 下的加密 blob 文件名）
    pub id: String,
    /// 原文件名（展示用）
    pub name: String,
    /// 宽（像素；解析失败为 0）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 移入时间（Unix 秒）
    pub added_ts: u64,
    /// 原始完整路径（恢复目标）
    pub original_path: String,
}

/// 列表返回条目（= 索引条目 + base64 缩略图）
#[derive(Debug, Clone, Serialize)]
pub struct VaultEntry {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub added_ts: u64,
    pub original_path: String,
    /// base64 PNG 缩略图（生成失败为空串，前端占位）
    pub thumb: String,
}

/// 索引明文结构
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VaultIndex {
    entries: Vec<VaultEntryCore>,
}

/// 解锁会话：主密钥 + 盐 + 已解密索引（仅存于 Rust 进程内，绝不下发前端）
#[derive(Clone)]
struct VaultSession {
    key: [u8; KEY_LEN],
    salt: [u8; SALT_LEN],
    entries: Vec<VaultEntryCore>,
}

static SESSION: OnceLock<Mutex<Option<VaultSession>>> = OnceLock::new();

fn session() -> &'static Mutex<Option<VaultSession>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

/// 缩略图 LRU 缓存：id → (base64 PNG, 最近使用时间戳)
static THUMBS: OnceLock<Mutex<HashMap<String, (String, u64)>>> = OnceLock::new();
/// 单调递增计数器（LRU 时间戳）
static TICK: AtomicU64 = AtomicU64::new(0);

// ==================== 命令（全部 async fn） ====================

/// 相册状态：是否初始化（index.json 存在）/ 是否解锁 / 条目数（未解锁为 0）
#[tauri::command]
pub async fn vault_status() -> Result<VaultStatus, String> {
    let initialized = index_path()?.exists();
    let (unlocked, count) = {
        let guard = session().lock().map_err(|e| format!("会话锁失败: {}", e))?;
        match guard.as_ref() {
            Some(s) => (true, s.entries.len()),
            None => (false, 0),
        }
    };
    Ok(VaultStatus {
        initialized,
        unlocked,
        count,
    })
}

/// 初始化（已初始化则 Err）：新盐 + 派生密钥 + 写入空加密索引，并直接进入解锁态
#[tauri::command]
pub async fn vault_setup(password: String) -> Result<(), String> {
    if index_path()?.exists() {
        return Err("私密相册已初始化，如需更换口令请使用修改密码".into());
    }
    if password.is_empty() {
        return Err("密码不能为空".into());
    }
    data_dir()?; // 建目录

    let (salt, key) =
        tokio::task::spawn_blocking(move || -> Result<([u8; SALT_LEN], [u8; KEY_LEN]), String> {
            let mut salt = [0u8; SALT_LEN];
            random_fill(&mut salt);
            let key = derive_key(&password, &salt);
            write_index_raw(&salt, &key, &[])?;
            Ok((salt, key))
        })
        .await
        .map_err(|e| format!("初始化任务失败: {}", e))??;

    *session().lock().map_err(|e| format!("会话锁失败: {}", e))? = Some(VaultSession {
        key,
        salt,
        entries: Vec::new(),
    });
    log::info!("私密相册初始化完成");
    Ok(())
}

/// 解锁：读索引文件 → 盐派生 → 解密验证；密码错误返回 Err，成功进入解锁态
#[tauri::command]
pub async fn vault_unlock(password: String) -> Result<bool, String> {
    let (salt_vec, nonce, ct) = read_index_raw()?;
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&salt_vec);
    let (key, entries) = tokio::task::spawn_blocking(
        move || -> Result<([u8; KEY_LEN], Vec<VaultEntryCore>), String> {
            let key = derive_key(&password, &salt);
            // GCM 认证失败 = 口令不对（或数据损坏），统一报“密码错误”
            let entries = decrypt_index(&key, &nonce, &ct).map_err(|_| "密码错误".to_string())?;
            Ok((key, entries))
        },
    )
    .await
    .map_err(|e| format!("解锁任务失败: {}", e))??;

    *session().lock().map_err(|e| format!("会话锁失败: {}", e))? =
        Some(VaultSession { key, salt, entries });
    log::info!("私密相册已解锁");
    Ok(true)
}

/// 锁定：清零并丢弃进程内主密钥 + 清空查看目录 + 清缩略图缓存
#[tauri::command]
pub async fn vault_lock() -> Result<(), String> {
    {
        let mut guard = session().lock().map_err(|e| format!("会话锁失败: {}", e))?;
        if let Some(mut s) = guard.take() {
            s.key.fill(0);
            s.salt.fill(0);
        }
    }
    // 清空 %TEMP%\jietu-hdr\vault_view\（残留明文查看文件）
    let vd = view_dir();
    if vd.exists() {
        if let Err(e) = std::fs::remove_dir_all(&vd) {
            log::warn!("清空查看目录失败: {:#}", e);
        }
    }
    // 缩略图缓存同样含已解密内容，一并清空
    if let Some(c) = THUMBS.get() {
        if let Ok(mut m) = c.lock() {
            m.clear();
        }
    }
    log::info!("私密相册已锁定");
    Ok(())
}

/// 移入：逐个 读文件→解析宽高→加密→写 data\<id>；全部成功后更新加密索引并删除源文件
/// （= 移入语义）；源文件删除失败的条目回滚（保持“库内=已移入”一致）。返回成功数
#[tauri::command]
pub async fn vault_add(paths: Vec<String>) -> Result<usize, String> {
    let sess = session_snapshot()?;
    let key = sess.key;

    let prepared = tokio::task::spawn_blocking(move || -> Result<Vec<VaultEntryCore>, String> {
        let mut out = Vec::new();
        let ts = now_secs();
        for p in &paths {
            let bytes = match std::fs::read(p) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("vault_add 跳过（读取失败）{}: {:#}", p, e);
                    continue;
                }
            };
            // 源文件尚在，直接读头解析宽高
            let (w, h) = decode::quick_dimensions(Path::new(p));
            let id = random_id();
            if let Err(e) = write_blob(&id, &key, &bytes) {
                log::warn!("vault_add 跳过（加密写入失败）{}: {}", p, e);
                continue;
            }
            let name = Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| id.clone());
            out.push(VaultEntryCore {
                id,
                name,
                width: w,
                height: h,
                added_ts: ts,
                original_path: p.clone(),
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| format!("移入任务失败: {}", e))??;

    if prepared.is_empty() {
        return Ok(0);
    }

    // 先提交索引（追加），再删源文件：中途崩溃最多“库内外各一份”，不丢数据
    {
        let mut guard = session().lock().map_err(|e| format!("会话锁失败: {}", e))?;
        let s = guard.as_mut().ok_or_else(|| "私密相册未解锁".to_string())?;
        s.entries.extend(prepared.iter().cloned());
        write_index_raw(&s.salt, &s.key, &s.entries)?;
    }

    let mut ok_count = 0usize;
    let mut rollback: Vec<String> = Vec::new();
    for e in &prepared {
        match std::fs::remove_file(&e.original_path) {
            Ok(()) => ok_count += 1,
            Err(err) => {
                log::warn!("移入后删除源文件失败 {}: {:#}", e.original_path, err);
                rollback.push(e.id.clone());
            }
        }
    }
    if !rollback.is_empty() {
        // 回滚删除不了源文件的条目（删 blob + 移出索引）
        let data = data_dir()?;
        let mut guard = session().lock().map_err(|e| format!("会话锁失败: {}", e))?;
        if let Some(s) = guard.as_mut() {
            for id in &rollback {
                let _ = std::fs::remove_file(data.join(id));
            }
            s.entries.retain(|e| !rollback.contains(&e.id));
            write_index_raw(&s.salt, &s.key, &s.entries)?;
        }
    }
    log::info!("私密相册移入 {} 项", ok_count);
    Ok(ok_count)
}

/// 列表（仅解锁态）：索引条目 + base64 缩略图（LRU ≤ 50，生成失败为空串）
#[tauri::command]
pub async fn vault_list() -> Result<Vec<VaultEntry>, String> {
    let sess = session_snapshot()?;
    let key = sess.key;
    let entries = sess.entries.clone();

    let list = tokio::task::spawn_blocking(move || -> Vec<VaultEntry> {
        entries
            .into_iter()
            .map(|e| {
                let thumb = cached_thumb(&e.id, &key).unwrap_or_default();
                VaultEntry {
                    id: e.id,
                    name: e.name,
                    width: e.width,
                    height: e.height,
                    added_ts: e.added_ts,
                    original_path: e.original_path,
                    thumb,
                }
            })
            .collect()
    })
    .await
    .map_err(|e| format!("列表任务失败: {}", e))?;
    Ok(list)
}

/// 解密到 %TEMP%\jietu-hdr\vault_view\<id>_<原名> 供看图（保留扩展名）
#[tauri::command]
pub async fn vault_open(id: String) -> Result<String, String> {
    let sess = session_snapshot()?;
    let entry = sess
        .entries
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .ok_or_else(|| "条目不存在".to_string())?;
    let key = sess.key;

    let out_path = tokio::task::spawn_blocking(move || -> Result<PathBuf, String> {
        let bytes = read_blob(&id, &key)?;
        let dir = view_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建查看目录失败: {:#}", e))?;
        let p = dir.join(format!("{}_{}", id, sanitize_filename(&entry.name)));
        std::fs::write(&p, bytes).map_err(|e| format!("写出查看文件失败: {:#}", e))?;
        Ok(p)
    })
    .await
    .map_err(|e| format!("打开任务失败: {}", e))??;
    Ok(out_path.to_string_lossy().into_owned())
}

/// 恢复到 original_path（仅解锁态）：解密写回原位置后移出相册（删 blob + 更新索引）
#[tauri::command]
pub async fn vault_restore(id: String) -> Result<String, String> {
    let sess = session_snapshot()?;
    let entry = sess
        .entries
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .ok_or_else(|| "条目不存在".to_string())?;
    if entry.original_path.is_empty() {
        return Err("该条目没有原始路径信息，无法恢复".into());
    }
    if Path::new(&entry.original_path).exists() {
        return Err(format!("目标位置已存在文件: {}", entry.original_path));
    }
    let key = sess.key;
    let blob_id = id.clone(); // 闭包内使用的副本，外层 id 留作后续索引更新

    let (restored, blob) =
        tokio::task::spawn_blocking(move || -> Result<(String, PathBuf), String> {
            let bytes = read_blob(&blob_id, &key)?;
            if let Some(parent) = Path::new(&entry.original_path).parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目标目录失败: {:#}", e))?;
            }
            std::fs::write(&entry.original_path, &bytes)
                .map_err(|e| format!("恢复写出失败: {:#}", e))?;
            Ok((entry.original_path.clone(), blob_path(&blob_id)?))
        })
        .await
        .map_err(|e| format!("恢复任务失败: {}", e))??;

    {
        let mut guard = session().lock().map_err(|e| format!("会话锁失败: {}", e))?;
        if let Some(s) = guard.as_mut() {
            let _ = std::fs::remove_file(&blob);
            s.entries.retain(|e| e.id != id);
            write_index_raw(&s.salt, &s.key, &s.entries)?;
        }
    }
    log::info!("已从私密相册恢复: {}", restored);
    Ok(restored)
}

/// 永久删除单条（删加密 blob + 更新索引 + 清该项缩略图缓存）
#[tauri::command]
pub async fn vault_remove(id: String) -> Result<(), String> {
    let sess = session_snapshot()?;
    if !sess.entries.iter().any(|e| e.id == id) {
        return Err("条目不存在".into());
    }
    let blob = blob_path(&id)?;
    {
        let mut guard = session().lock().map_err(|e| format!("会话锁失败: {}", e))?;
        if let Some(s) = guard.as_mut() {
            if let Err(e) = std::fs::remove_file(&blob) {
                log::warn!("删除加密数据失败 {}: {:#}", id, e);
            }
            s.entries.retain(|e| e.id != id);
            write_index_raw(&s.salt, &s.key, &s.entries)?;
        }
    }
    if let Some(c) = THUMBS.get() {
        if let Ok(mut m) = c.lock() {
            m.remove(&id);
        }
    }
    log::info!("私密相册已删除条目: {}", id);
    Ok(())
}

/// 修改密码：旧口令解索引校验 → 新盐/新密钥 → 两阶段重加密全部 blob → 新索引落盘。
/// 任一步失败整体回退到旧口令状态；成功后若处于解锁态则无缝切换到新密钥
#[tauri::command]
pub async fn vault_change_password(oldPw: String, newPw: String) -> Result<(), String> {
    if newPw.is_empty() {
        return Err("新密码不能为空".into());
    }
    let (salt_vec, nonce, ct) = read_index_raw()?;

    let (new_salt, new_key, entries) = tokio::task::spawn_blocking(
        move || -> Result<([u8; SALT_LEN], [u8; KEY_LEN], Vec<VaultEntryCore>), String> {
            // 1. 旧口令校验 + 解出索引
            let mut salt = [0u8; SALT_LEN];
            salt.copy_from_slice(&salt_vec);
            let old_key = derive_key(&oldPw, &salt);
            let entries =
                decrypt_index(&old_key, &nonce, &ct).map_err(|_| "旧密码错误".to_string())?;

            // 2. 新盐 + 新密钥
            let mut new_salt = [0u8; SALT_LEN];
            random_fill(&mut new_salt);
            let new_key = derive_key(&newPw, &new_salt);

            let dir = data_dir()?;
            // 3. 阶段一：全部 blob 用新密钥重加密为 <id>.tmp（原数据不动）
            for e in &entries {
                let plain = read_blob(&e.id, &old_key)?;
                let (n, c) = encrypt_bytes(&new_key, &plain)?;
                let mut buf = Vec::with_capacity(NONCE_LEN + c.len());
                buf.extend_from_slice(&n);
                buf.extend_from_slice(&c);
                if let Err(err) = std::fs::write(dir.join(format!("{}.tmp", e.id)), &buf) {
                    cleanup_temp(&dir, &entries);
                    return Err(format!("重加密写出失败: {:#}", err));
                }
            }
            // 4. 阶段二：旧数据让位为 <id>.old（失败则还原已让位项）
            let mut moved: Vec<&VaultEntryCore> = Vec::new();
            for e in &entries {
                if let Err(err) =
                    std::fs::rename(dir.join(&e.id), dir.join(format!("{}.old", e.id)))
                {
                    rollback_move(&dir, &moved);
                    cleanup_temp(&dir, &entries);
                    return Err(format!("备份数据失败: {:#}", err));
                }
                moved.push(e);
            }
            // 5. 阶段三：新数据就位（失败则用 .old 还原全部）
            for e in &entries {
                if let Err(err) =
                    std::fs::rename(dir.join(format!("{}.tmp", e.id)), dir.join(&e.id))
                {
                    rollback_move(&dir, &moved);
                    cleanup_temp(&dir, &entries);
                    return Err(format!("替换加密数据失败: {:#}", err));
                }
            }
            // 6. 阶段四：删 .old + 写新索引（此后整套数据均由新口令保护）
            for e in &entries {
                let _ = std::fs::remove_file(dir.join(format!("{}.old", e.id)));
            }
            write_index_raw(&new_salt, &new_key, &entries)?;
            Ok((new_salt, new_key, entries))
        },
    )
    .await
    .map_err(|e| format!("改密任务失败: {}", e))??;

    // 已解锁则无缝切换到新密钥（索引内容与磁盘一致）
    {
        let mut guard = session().lock().map_err(|e| format!("会话锁失败: {}", e))?;
        if let Some(s) = guard.as_mut() {
            s.key = new_key;
            s.salt = new_salt;
            s.entries = entries;
        }
    }
    log::info!("私密相册已修改密码");
    Ok(())
}

// ==================== 路径 ====================

/// 相册根目录（%APPDATA%\jietu-hdr\vault）
fn vault_dir() -> Result<PathBuf, String> {
    let base = dirs::config_dir()
        .map(|d| d.join("jietu-hdr").join("vault"))
        .ok_or_else(|| "无法定位配置目录".to_string())?;
    std::fs::create_dir_all(&base).map_err(|e| format!("{:#}", e))?;
    Ok(base)
}

/// 加密数据目录（vault\data）
fn data_dir() -> Result<PathBuf, String> {
    let d = vault_dir()?.join("data");
    std::fs::create_dir_all(&d).map_err(|e| format!("{:#}", e))?;
    Ok(d)
}

/// 加密索引路径（vault\index.json）
fn index_path() -> Result<PathBuf, String> {
    Ok(vault_dir()?.join("index.json"))
}

/// 明文查看目录（%TEMP%\jietu-hdr\vault_view，vault_lock 清空）
fn view_dir() -> PathBuf {
    std::env::temp_dir().join("jietu-hdr").join("vault_view")
}

/// 加密 blob 路径（vault\data\<id>）
fn blob_path(id: &str) -> Result<PathBuf, String> {
    Ok(data_dir()?.join(id))
}

/// 当前 Unix 秒
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 文件名净化（防御性替换 Windows 非法字符）
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

// ==================== 加密 ====================

/// PBKDF2-HMAC-SHA256 派生 32 字节主密钥
fn derive_key(password: &str, salt: &[u8]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, PBKDF2_ITERS, &mut key);
    key
}

/// 系统熵源填充随机字节（aes-gcm 默认特性自带的 OsRng）
fn random_fill(buf: &mut [u8]) {
    use aes_gcm::aead::rand_core::RngCore;
    aes_gcm::aead::OsRng.fill_bytes(buf);
}

/// 随机 32 位十六进制 id（= data\ 下 blob 文件名）
fn random_id() -> String {
    let mut b = [0u8; 16];
    random_fill(&mut b);
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// AES-256-GCM 加密：返回 (nonce, 密文+tag)
fn encrypt_bytes(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut nonce = vec![0u8; NONCE_LEN];
    random_fill(&mut nonce);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| "AES-GCM 加密失败".to_string())?;
    Ok((nonce, ct))
}

/// AES-256-GCM 解密（认证失败 = 密钥错误或数据损坏）
fn decrypt_bytes(key: &[u8; KEY_LEN], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| "AES-GCM 解密失败（密钥错误或数据损坏）".to_string())
}

// ==================== 索引读写（整体加密） ====================

/// 读取索引文件 → (盐, nonce, 密文)；未初始化报错
fn read_index_raw() -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    let p = index_path()?;
    if !p.exists() {
        return Err("私密相册尚未初始化".into());
    }
    let raw = std::fs::read(&p).map_err(|e| format!("读取索引失败: {:#}", e))?;
    if raw.len() < SALT_LEN + NONCE_LEN + TAG_LEN {
        return Err("索引文件损坏".into());
    }
    Ok((
        raw[..SALT_LEN].to_vec(),
        raw[SALT_LEN..SALT_LEN + NONCE_LEN].to_vec(),
        raw[SALT_LEN + NONCE_LEN..].to_vec(),
    ))
}

/// 序列化 + 加密 + 写入索引文件（帧：[盐][nonce][密文]）
fn write_index_raw(
    salt: &[u8],
    key: &[u8; KEY_LEN],
    entries: &[VaultEntryCore],
) -> Result<(), String> {
    let json = serde_json::to_vec(&VaultIndex {
        entries: entries.to_vec(),
    })
    .map_err(|e| format!("{:#}", e))?;
    let (nonce, ct) = encrypt_bytes(key, &json)?;
    let mut buf = Vec::with_capacity(SALT_LEN + NONCE_LEN + ct.len());
    buf.extend_from_slice(salt);
    buf.extend_from_slice(&nonce);
    buf.extend_from_slice(&ct);
    std::fs::write(index_path()?, buf).map_err(|e| format!("写入索引失败: {:#}", e))
}

/// 解密并解析索引
fn decrypt_index(
    key: &[u8; KEY_LEN],
    nonce: &[u8],
    ct: &[u8],
) -> Result<Vec<VaultEntryCore>, String> {
    let plain = decrypt_bytes(key, nonce, ct)?;
    let idx: VaultIndex = serde_json::from_slice(&plain).map_err(|_| "索引数据损坏".to_string())?;
    Ok(idx.entries)
}

// ==================== blob 读写 ====================

/// 读取并解密 blob（帧：[nonce][密文]）
fn read_blob(id: &str, key: &[u8; KEY_LEN]) -> Result<Vec<u8>, String> {
    let p = blob_path(id)?;
    let raw = std::fs::read(&p).map_err(|e| format!("读取加密数据失败: {:#}", e))?;
    if raw.len() < NONCE_LEN + TAG_LEN {
        return Err("加密数据损坏".into());
    }
    decrypt_bytes(key, &raw[..NONCE_LEN], &raw[NONCE_LEN..])
}

/// 加密并写入 blob
fn write_blob(id: &str, key: &[u8; KEY_LEN], plain: &[u8]) -> Result<(), String> {
    let (nonce, ct) = encrypt_bytes(key, plain)?;
    let mut buf = Vec::with_capacity(NONCE_LEN + ct.len());
    buf.extend_from_slice(&nonce);
    buf.extend_from_slice(&ct);
    std::fs::write(blob_path(id)?, buf).map_err(|e| format!("写入加密数据失败: {:#}", e))
}

// ==================== 会话辅助 ====================

/// 会话快照（未解锁报错）；锁只在克隆时短持有，不跨 await
fn session_snapshot() -> Result<VaultSession, String> {
    session()
        .lock()
        .map_err(|e| format!("会话锁失败: {}", e))?
        .clone()
        .ok_or_else(|| "私密相册未解锁".to_string())
}

// ==================== 缩略图（复用 thumbnail.rs 思路：LRU ≤ 50） ====================

/// 取缩略图 base64 PNG：命中缓存直接返回，未命中 解密→内存解码→缩放→PNG base64
fn cached_thumb(id: &str, key: &[u8; KEY_LEN]) -> Result<String, String> {
    let cache = THUMBS.get_or_init(|| Mutex::new(HashMap::new()));
    let tick = TICK.fetch_add(1, Ordering::Relaxed);

    // 1. 命中：刷新 LRU 时间戳后返回
    {
        let mut m = cache.lock().map_err(|e| format!("缓存锁失败: {}", e))?;
        if let Some((b64, last)) = m.get_mut(id) {
            *last = tick;
            return Ok(b64.clone());
        }
    }

    // 2. 未命中：生成（锁外执行，避免长时间持锁）
    let b64 = gen_thumb(id, key)?;

    // 3. 写入缓存（超上限先淘汰最久未使用项）
    {
        let mut m = cache.lock().map_err(|e| format!("缓存锁失败: {}", e))?;
        while m.len() >= THUMB_LRU_MAX {
            let oldest = m
                .iter()
                .min_by_key(|(_, (_, last))| *last)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    m.remove(&k);
                }
                None => break,
            }
        }
        m.insert(id.to_string(), (b64.clone(), tick));
    }
    Ok(b64)
}

/// 解密到内存 → 解码 → 200×200 保比例 → PNG base64
/// （png/jpg/gif/bmp/webp/tiff/ico 可解；AVIF/PSD 内存解码受限返回 Err 由前端占位）
fn gen_thumb(id: &str, key: &[u8; KEY_LEN]) -> Result<String, String> {
    let bytes = read_blob(id, key)?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("解码失败: {}", e))?;
    let thumb = img.thumbnail(THUMB_SIZE, THUMB_SIZE);

    let mut buf = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| format!("缩略图编码失败: {}", e))?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&buf))
}

// ==================== 改密两阶段辅助 ====================

/// 清理改密临时文件（<id>.tmp）
fn cleanup_temp(dir: &Path, entries: &[VaultEntryCore]) {
    for e in entries {
        let _ = std::fs::remove_file(dir.join(format!("{}.tmp", e.id)));
    }
}

/// 还原已让位的旧数据（<id>.old → <id>）
fn rollback_move(dir: &Path, moved: &[&VaultEntryCore]) {
    for e in moved {
        let _ = std::fs::rename(dir.join(format!("{}.old", e.id)), dir.join(&e.id));
    }
}
